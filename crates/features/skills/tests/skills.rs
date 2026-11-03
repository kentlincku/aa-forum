use aaf_api::{AuthFeature, HttpFeature};
use aaf_core::feature::BootOptions;
use aaf_core::storage::Db;
use aaf_core::{Core, Registry};
use aaf_feature_skills::{Assign, Skills, Store};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SK: &str =
    "---\nname: review\ndescription: |\n  Review code\n  carefully\n---\nAlways check tests.\n";

fn a(skills: &[&str], packs: &[&str]) -> Assign {
    Assign {
        skills: skills.iter().map(|s| s.to_string()).collect(),
        packs: packs.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn store_crud_history_packs_compose() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::new(d.path());
    // 名稱規則
    for bad in ["", "Bad", "-x", "a/b", "..", &"x".repeat(65)] {
        assert_eq!(
            s.save(bad, "x", "alice", true, "").unwrap_err().0,
            400,
            "{bad}"
        );
    }
    assert_eq!(s.save("empty", "  ", "alice", true, "").unwrap_err().0, 400);
    assert_eq!(
        s.save("big", &"x".repeat(200_001), "alice", true, "")
            .unwrap_err()
            .0,
        413
    );
    s.save("review", SK, "alice", true, "").unwrap();
    assert_eq!(s.save("review", SK, "alice", true, "").unwrap_err().0, 409);
    assert_eq!(s.save("nope", SK, "alice", false, "").unwrap_err().0, 404);
    assert_eq!(
        s.save("review", SK, "alice", false, "").unwrap()["changed"],
        false,
        "unchanged is a no-op"
    );
    s.save("review", "v2 body", "alice", false, "tighten")
        .unwrap();
    s.save(
        "deploy",
        "---\ndescription: Ship it\n---\nsteps",
        "alice",
        true,
        "",
    )
    .unwrap();
    s.save("style", "no frontmatter", "alice", true, "")
        .unwrap();
    // 清單與說明
    let l = s.listing();
    assert_eq!(
        l.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        ["deploy", "review", "style"]
    );
    assert_eq!(
        aaf_feature_skills::store::description(SK),
        "Review code carefully"
    );
    // 歷史與還原
    let (vers, log) = s.history("review").unwrap();
    assert_eq!(vers.len(), 1);
    assert_eq!(
        log.iter()
            .map(|e| e["action"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["create", "edit"]
    );
    assert_eq!(s.read_version("review", &vers[0].version).unwrap(), SK);
    assert_eq!(s.read_version("review", "../../x").unwrap_err().0, 400);
    s.restore("review", &vers[0].version, "ann").unwrap();
    assert_eq!(s.read("review").unwrap(), SK);
    assert_eq!(
        s.history("review").unwrap().0.len(),
        2,
        "restore snapshots the current version first"
    );
    // 包與指派
    assert_eq!(
        s.set_pack("build", &["nope".into()], "alice")
            .unwrap_err()
            .0,
        400
    );
    s.set_pack("build", &["deploy".into(), "review".into()], "alice")
        .unwrap();
    assert_eq!(
        s.assign("role", "coder", &a(&["ghost"], &[]), "alice")
            .unwrap_err()
            .0,
        400
    );
    s.assign(
        "role",
        "coder",
        &a(&["style", "review"], &["build"]),
        "alice",
    )
    .unwrap();
    s.assign("room", "7", &a(&["style"], &[]), "alice").unwrap();
    // 組合：角色 skills → 角色包 → 群；去重
    assert_eq!(s.compose("coder", None), ["style", "review", "deploy"]);
    assert_eq!(s.compose("coder", Some(7)), ["style", "review", "deploy"]);
    assert_eq!(s.compose("other", Some(7)), ["style"]);
    // 群級 lead：額外的包只加在這次組合，不影響其他群／角色
    assert_eq!(
        s.compose_with("other", Some(7), &["build"], &[]),
        ["style", "deploy", "review"]
    );
    assert_eq!(s.compose("other", Some(8)), Vec::<String>::new());
    // 群已指定別人當 lead：拿掉角色指派裡的包，直接指派的 skill 不受影響
    assert_eq!(
        s.compose_with("coder", None, &[], &["build"]),
        ["style", "review"]
    );
    let ctx = s.context("other", Some(7));
    assert!(ctx.contains("[Skill assigned to you: style]\nno frontmatter"));
    assert!(
        ctx.contains("- deploy: Ship it (") && ctx.contains("- review: Review code carefully ("),
        "{ctx}"
    );
    assert!(!ctx.contains("- style:"), "assigned skill not listed again");
    // 使用中不可刪
    assert_eq!(s.delete("review", "alice").unwrap_err().0, 409);
    assert_eq!(s.delete_pack("build", "alice").unwrap_err().0, 409);
    s.assign("role", "coder", &a(&[], &[]), "alice").unwrap();
    s.delete_pack("build", "alice").unwrap();
    s.delete("review", "alice").unwrap();
    assert_eq!(s.read("review").unwrap_err().0, 404);
    assert!(
        !s.history("review").unwrap().0.is_empty(),
        "history kept after delete"
    );
    // symlink 的 skill 不載入
    std::fs::create_dir_all(d.path().join("outside")).unwrap();
    std::fs::write(d.path().join("outside/SKILL.md"), "secret").unwrap();
    std::os::unix::fs::symlink(d.path().join("outside"), d.path().join("skills/linked")).unwrap();
    assert!(!s.ids().contains(&"linked".to_string()));
    std::fs::create_dir_all(d.path().join("skills/filelink")).unwrap();
    std::os::unix::fs::symlink(
        d.path().join("outside/SKILL.md"),
        d.path().join("skills/filelink/SKILL.md"),
    )
    .unwrap();
    assert!(!s.ids().contains(&"filelink".to_string()));
    assert_eq!(s.read("filelink").unwrap_err().0, 404);
}

async fn call(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let b = body.map(|v| v.to_string()).unwrap_or_default();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len()).as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let body = if head.to_ascii_lowercase().contains("chunked") {
        body.lines()
            .filter(|l| !l.is_empty() && usize::from_str_radix(l.trim(), 16).is_err())
            .collect::<Vec<_>>()
            .join("")
    } else {
        body.to_string()
    };
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(&body).unwrap_or(Value::Null),
    )
}

struct Wrap(Arc<Skills>);
impl aaf_core::Feature for Wrap {
    fn name(&self) -> &'static str {
        aaf_core::Feature::name(&*self.0)
    }
}

#[tokio::test]
async fn http_humans_edit_agents_read() {
    let d = tempfile::tempdir().unwrap();
    let skills = Arc::new(Skills::new(d.path()));
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Wrap(skills.clone()))).unwrap();
    let core = Core::boot(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    let agent = aaf_api::issue_role_token(&core, "coder", "lead").unwrap();
    let http: Vec<Arc<dyn HttpFeature>> = vec![skills];
    let app = aaf_api::app(core, &http);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let t = call(
        &addr,
        "POST",
        "/api/v1/auth/setup",
        None,
        Some(json!({"name": "alice", "password": "correct horse"})),
    )
    .await
    .1["token"]
        .as_str()
        .unwrap()
        .to_string();
    call(
        &addr,
        "POST",
        "/api/v1/auth/users",
        Some(&t),
        Some(json!({"name": "ann", "password": "password123"})),
    )
    .await;
    let ann = call(
        &addr,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"name": "ann", "password": "password123"})),
    )
    .await
    .1["token"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(
        call(
            &addr,
            "POST",
            "/api/v1/skills/items",
            Some(&t),
            Some(json!({"id": "review", "content": SK}))
        )
        .await
        .0,
        200
    );
    // 一般人類也能改（同 Python 版：rank human）
    assert_eq!(
        call(
            &addr,
            "PUT",
            "/api/v1/skills/items/review",
            Some(&ann),
            Some(json!({"content": "v2", "note": "n"}))
        )
        .await
        .0,
        200
    );
    // agent 只能讀
    assert_eq!(
        call(
            &addr,
            "GET",
            "/api/v1/skills/items/review",
            Some(&agent),
            None
        )
        .await
        .1["content"],
        "v2"
    );
    for (m, p, b) in [
        (
            "POST",
            "/api/v1/skills/items",
            json!({"id": "x", "content": "x"}),
        ),
        (
            "PUT",
            "/api/v1/skills/items/review",
            json!({"content": "hack"}),
        ),
        ("DELETE", "/api/v1/skills/items/review", json!({})),
        ("PUT", "/api/v1/skills/packs/p", json!({"skills": []})),
        (
            "PUT",
            "/api/v1/skills/assign/role/coder",
            json!({"skills": ["review"]}),
        ),
    ] {
        assert_eq!(
            call(&addr, m, p, Some(&agent), Some(b)).await.0,
            403,
            "{m} {p}"
        );
    }
    assert_eq!(
        call(
            &addr,
            "PUT",
            "/api/v1/skills/assign/role/coder",
            Some(&t),
            Some(json!({"skills": ["review"]}))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            &addr,
            "PUT",
            "/api/v1/skills/assign/room/abc",
            Some(&t),
            Some(json!({}))
        )
        .await
        .0,
        400
    );
    let (_, p) = call(
        &addr,
        "GET",
        "/api/v1/skills/preview?role=coder&room=3",
        Some(&t),
        None,
    )
    .await;
    assert_eq!(p["skills"], json!(["review"]));
    assert_eq!(
        call(
            &addr,
            "DELETE",
            "/api/v1/skills/items/review",
            Some(&t),
            None
        )
        .await
        .0,
        409
    );
    let (_, h) = call(
        &addr,
        "GET",
        "/api/v1/skills/items/review/history",
        Some(&t),
        None,
    )
    .await;
    assert_eq!(h["versions"].as_array().unwrap().len(), 1);
    let (_, list) = call(&addr, "GET", "/api/v1/skills/items", Some(&ann), None).await;
    assert_eq!(list["skills"][0]["used_by"], json!(["role coder"]));
}
