use aaf_api::{AuthFeature, HttpFeature};
use aaf_core::feature::BootOptions;
use aaf_core::storage::Db;
use aaf_core::{Core, Registry};
use aaf_feature_agents::Agents;
use aaf_feature_chat::Chat;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Wrap<T>(Arc<T>);
impl<T: aaf_core::Feature> aaf_core::Feature for Wrap<T> {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn migrations(&self) -> &'static [aaf_core::storage::Migration] {
        self.0.migrations()
    }
    fn capabilities(&self) -> &'static [aaf_core::perms::CapabilityDef] {
        self.0.capabilities()
    }
    fn config(&self) -> &'static [aaf_core::config::ConfigKey] {
        self.0.config()
    }
    fn subscribers(&self) -> Vec<Arc<dyn aaf_core::feature::Subscriber>> {
        self.0.subscribers()
    }
    fn jobs(&self) -> Vec<aaf_core::jobs::Job> {
        self.0.jobs()
    }
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

/// 假 npm：`view` 回套件資訊；`install` 在 node_modules/.bin 放一個包裝假 ACP agent 的執行檔。
fn fake_npm(dir: &std::path::Path, fake_acp: &str) -> std::path::PathBuf {
    let p = dir.join("npm");
    std::fs::write(
        &p,
        format!(
            r#"#!/bin/sh
case "$1" in
  view)
    case "$2" in
      fake-acp*) echo '{{"name":"fake-acp","version":"1.2.3","bin":{{"fake-acp":"bin.js"}}}}' ;;
      no-bin*) echo '{{"name":"no-bin","version":"1.0.0"}}' ;;
      @zed-industries/*) echo '{{"version":"0.16.0","deprecated":"replaced by @agentclientprotocol/codex-acp"}}' ;;
      @agentclientprotocol/*|pi-acp*) echo '"9.9.9"' ;;
      fakecli*) echo '"2.0.0"' ;;
      *) echo "404 not found" >&2; exit 1 ;;
    esac ;;
  install)
    pkg=$(eval echo \${{$#}})
    case "$pkg" in fakecli@*)
      v=${{pkg#fakecli@}}
      if [ "$2" = "-g" ]; then
        while [ "$1" != "--prefix" ]; do shift; done; dir="$2/lib/node_modules/fakecli"
      else
        dir="node_modules/fakecli"; mkdir -p node_modules/.bin
      fi
      mkdir -p "$dir"; printf '#!/bin/sh\necho "fakecli %s"\n' "$v" > "$dir/cli.sh"; chmod +x "$dir/cli.sh"
      [ "$2" = "-g" ] || ln -sf "../fakecli/cli.sh" node_modules/.bin/fakecli
      echo "added fakecli $v"; exit 0 ;;
    esac
    echo "installing $pkg"
    case "$pkg@$PWD" in
      *0.0.0-vendorfail@*/vendor/acp) echo "npm ERR! disk full" >&2; exit 1 ;;
    esac
    case "$pkg" in
      slow*) sleep 3 ;;
      broken*) echo "npm ERR! boom" >&2; exit 1 ;;
    esac
    name=$(echo "$pkg" | sed 's/\(.\)@[^@\/]*$/\1/')
    ver=${{pkg##*@}}
    mkdir -p node_modules/.bin "node_modules/$name"
    echo "{{\"name\":\"$name\",\"version\":\"$ver\"}}" > "node_modules/$name/package.json"
    for b in fake-acp codex-acp; do
      case "$ver" in
        0.0.0-bad) printf '#!/bin/sh\nexit 3\n' > node_modules/.bin/$b ;;
        *) printf '#!/bin/sh\nFAKE_MODELS=models exec {fake_acp} "$@"\n' > node_modules/.bin/$b ;;
      esac
      chmod +x node_modules/.bin/$b
    done
    echo "added 1 package" ;;
  uninstall)
    pkg=$(eval echo \${{$#}})
    echo "removing $pkg"
    rm -rf "node_modules/$pkg"
    # 像真的 npm：舊、新套件的 bin 同名時，移除舊套件會連同 .bin 連結一起刪掉
    case "$pkg" in @zed-industries/codex-acp) rm -f node_modules/.bin/codex-acp ;; esac ;;
esac
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

struct Env {
    addr: String,
    owner: String,
    human: String,
    home: tempfile::TempDir,
}

async fn start() -> Env {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("deploy")).unwrap();
    std::fs::write(home.path().join("deploy/roles.json"), "{\"roles\": {}}").unwrap();
    let fake_acp = env!("CARGO_BIN_EXE_aaf-fake-acp-for-agents");
    let npm = fake_npm(home.path(), fake_acp);
    let roster = aaf_runtime::dispatcher::Roster::load(home.path()).unwrap();
    let chat = Arc::new(Chat::with_tick(roster, Duration::from_millis(50)));
    let agents = Arc::new(
        Agents::new(home.path(), chat.shared())
            .with_npm(npm)
            .with_probe_timeout(Duration::from_secs(10)),
    );
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Wrap(chat.clone()))).unwrap();
    reg.register(Box::new(Wrap(agents.clone()))).unwrap();
    let core = Core::boot(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    let http: Vec<Arc<dyn HttpFeature>> = vec![chat, agents];
    let app = aaf_api::app(core, &http);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let owner = call(
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
        Some(&owner),
        Some(json!({"name": "ann", "password": "battery staple", "rank": "human"})),
    )
    .await;
    let human = call(
        &addr,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"name": "ann", "password": "battery staple"})),
    )
    .await
    .1["token"]
        .as_str()
        .unwrap()
        .to_string();
    Env {
        addr,
        owner,
        human,
        home,
    }
}

async fn wait_job(e: &Env, id: &str) -> Value {
    for _ in 0..200 {
        let j = call(
            &e.addr,
            "GET",
            &format!("/api/v1/agents/install/{id}"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if j["state"] != "running" {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("install did not finish");
}

/// 多帳號：假 `claude` 依帳號資料夾裡有沒有 logged-in 檔案回報登入狀態（真的 CLI 由實測涵蓋）。
#[tokio::test]
async fn accounts_create_list_status_and_remove() {
    let e = start().await;
    let bin = e.home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
case "$*" in
  *stream-json*)
    echo '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.25,"resetsAt":1793354400},"seven_day":{"utilization":0.5,"resetsAt":1793577600}}}}'
    echo '{"type":"result","subtype":"success","is_error":false,"result":"ok"}' ;;
  *"/usage"*) echo 'Total cost:            $0.0000' ;;
  *) if [ -f "$CLAUDE_CONFIG_DIR/logged-in" ]; then echo '{"loggedIn":true,"authMethod":"claude.ai","email":"someone@example.com","subscriptionType":"max"}'; else echo '{"loggedIn":false,"authMethod":"none"}'; fi ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // 只這個測試用 claude；PATH 是行程全域，但別的測試都不找 claude
    let old = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old}", bin.display()));
    let a = |m: &'static str, p: String, b: Option<Value>| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move { call(&addr, m, &format!("/api/v1/agents{p}"), Some(&t), b).await }
    };
    // 只有 owner
    assert_eq!(
        call(
            &e.addr,
            "GET",
            "/api/v1/agents/accounts",
            Some(&e.human),
            None
        )
        .await
        .0,
        403
    );
    // 新增：id、agent、資料夾的檢查
    for bad in [
        json!({"id": "Bad Id", "agent": "claude"}),
        json!({"id": "default", "agent": "claude"}),
        json!({"id": "x", "agent": "pi"}),
        json!({"id": "x", "agent": "claude", "dir": "relative/dir"}),
        json!({"id": "x", "agent": "claude", "dir": "/tmp/../etc"}),
    ] {
        assert_eq!(
            a("POST", "/accounts".into(), Some(bad.clone())).await.0,
            400,
            "{bad}"
        );
    }
    let (st, c) = a(
        "POST",
        "/accounts".into(),
        Some(json!({"id": "work", "agent": "claude", "label": "Work"})),
    )
    .await;
    assert_eq!(st, 200, "{c}");
    let dir = std::path::PathBuf::from(c["dir"].as_str().unwrap());
    assert!(dir.starts_with(e.home.path().join("accounts")) && dir.is_dir());
    assert!(
        c["login_command"]
            .as_str()
            .unwrap()
            .contains("CLAUDE_CONFIG_DIR="),
        "{c}"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        a(
            "POST",
            "/accounts".into(),
            Some(json!({"id": "work", "agent": "claude"}))
        )
        .await
        .0,
        409,
        "duplicate id"
    );
    assert_eq!(
        a(
            "POST",
            "/accounts".into(),
            Some(json!({"id": "w2", "agent": "claude", "dir": dir}))
        )
        .await
        .0,
        409,
        "folder already used"
    );
    // 列表：沒登入 → false；放進 logged-in 之後 → true，信箱只回遮過的
    let find = |l: &Value, id: &str| {
        l["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == id)
            .cloned()
            .unwrap()
    };
    let l = a("GET", "/accounts".into(), None).await.1;
    assert_eq!(find(&l, "work")["status"]["logged_in"], false, "{l}");
    assert_eq!(find(&l, "default")["agent"], "claude");
    std::fs::write(dir.join("logged-in"), "").unwrap();
    let w = find(&a("GET", "/accounts".into(), None).await.1, "work");
    assert_eq!(
        (
            w["status"]["logged_in"].as_bool(),
            w["status"]["plan"].as_str()
        ),
        (Some(true), Some("max")),
        "{w}"
    );
    assert_eq!(w["status"]["email"], "s***@example.com", "email is masked");
    assert!(!w.to_string().contains("someone@example.com"));
    assert_eq!(w["label"], "Work");
    // 額度：`/usage` 沒給視窗（token 登入）→ 先沒有；按重新整理後從回合附帶的額度取得
    assert!(w["quota"]["windows"].as_array().unwrap().is_empty(), "{w}");
    assert_eq!(
        call(
            &e.addr,
            "POST",
            "/api/v1/agents/quota/refresh",
            Some(&e.human),
            Some(json!({"account": "work"}))
        )
        .await
        .0,
        403
    );
    let (st, r) = a(
        "POST",
        "/quota/refresh".into(),
        Some(json!({"account": "work"})),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(
        (
            r["account"].as_str(),
            r["windows"][0]["remaining_percent"].as_f64()
        ),
        (Some("work"), Some(75.0)),
        "{r}"
    );
    let w = find(&a("GET", "/accounts".into(), None).await.1, "work");
    assert_eq!(w["quota"]["from_turn"], true, "{w}");
    assert_eq!(w["quota"]["windows"][0]["label"], "5 hours", "{w}");
    assert_eq!(w["quota"]["windows"][1]["remaining_percent"], 50.0, "{w}");
    let d = find(&a("GET", "/accounts".into(), None).await.1, "default");
    assert!(
        d["quota"]["windows"]
            .as_array()
            .is_none_or(|w| w.is_empty()),
        "another account's reading is not shown for default: {d}"
    );
    // 指給別的 agent 的角色：擋下
    let (st, b) = a(
        "POST",
        "/roles/coder/engine".into(),
        Some(json!({"account": "work"})),
    )
    .await;
    assert!(st == 404 || st == 400, "{st} {b}");
    // 有角色在用就不能刪
    std::fs::write(
        e.home.path().join("deploy/roles.json"),
        json!({"roles": {"planner": {"driver": "acp", "acp_agent": "claude", "account": "work", "rank": "lead"}}}).to_string(),
    )
    .unwrap();
    let (st, b) = a("DELETE", "/accounts/work".into(), None).await;
    assert_eq!(st, 409, "{b}");
    assert!(b["message"].as_str().unwrap().contains("planner"), "{b}");
    std::fs::write(e.home.path().join("deploy/roles.json"), "{\"roles\": {}}").unwrap();
    let (st, b) = a("DELETE", "/accounts/work".into(), None).await;
    assert_eq!(st, 200, "{b}");
    assert!(
        dir.is_dir() && dir.join("logged-in").exists(),
        "the folder (and its login) is kept"
    );
    assert_eq!(a("DELETE", "/accounts/work".into(), None).await.0, 404);
    std::env::set_var("PATH", old);
}

#[tokio::test]
async fn install_probe_add_role_switch_model_remove() {
    let e = start().await;
    let a = |m: &'static str, p: String, b: Option<Value>| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move { call(&addr, m, &format!("/api/v1/agents{p}"), Some(&t), b).await }
    };
    // 權限：只有 Owner
    assert_eq!(
        call(
            &e.addr,
            "GET",
            "/api/v1/agents/catalog",
            Some(&e.human),
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &e.addr,
            "POST",
            "/api/v1/agents/install",
            Some(&e.human),
            Some(json!({"agent": "codex", "confirm": true}))
        )
        .await
        .0,
        403
    );
    // 目錄：內建 5 個（gemini 已移除、agy 加入），vendor 類未安裝
    let (st, cat) = a("GET", "/catalog".into(), None).await;
    assert_eq!(st, 200, "{cat}");
    let ids: Vec<&str> = cat["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["hermes", "claude", "codex", "pi", "agy"]);
    assert!(cat["agents"]
        .as_array()
        .unwrap()
        .iter()
        .all(|x| x["id"] == "hermes" || x["installed"] == false));

    // 自訂：計畫（不安裝）、名稱檢查、npm 查不到、沒有 bin
    let (st, plan) = a(
        "POST",
        "/plan-custom".into(),
        Some(json!({"npm": "fake-acp"})),
    )
    .await;
    assert_eq!(st, 200, "{plan}");
    assert_eq!(
        (
            plan["id"].as_str(),
            plan["npm"].as_str(),
            plan["acp"].as_str()
        ),
        (Some("fake-acp"), Some("fake-acp@1.2.3"), Some("fake-acp"))
    );
    assert_eq!(
        a(
            "POST",
            "/plan-custom".into(),
            Some(json!({"npm": "-g evil"}))
        )
        .await
        .0,
        400
    );
    assert!(a(
        "POST",
        "/plan-custom".into(),
        Some(json!({"npm": "missing-pkg"}))
    )
    .await
    .1["message"]
        .as_str()
        .unwrap()
        .contains("cannot find"));
    assert!(a(
        "POST",
        "/plan-custom".into(),
        Some(json!({"npm": "no-bin"}))
    )
    .await
    .1["message"]
        .as_str()
        .unwrap()
        .contains("no executable"));

    // 安裝：沒有 confirm 拒絕；成功後目錄變成已安裝
    assert_eq!(
        a("POST", "/install".into(), Some(json!({"npm": "fake-acp"})))
            .await
            .0,
        400
    );
    assert_eq!(
        a(
            "POST",
            "/install".into(),
            Some(json!({"agent": "hermes", "confirm": true}))
        )
        .await
        .0,
        400,
        "hermes is not installed by us"
    );
    let (st, job) = a(
        "POST",
        "/install".into(),
        Some(json!({"npm": "fake-acp", "confirm": true})),
    )
    .await;
    assert_eq!(st, 200, "{job}");
    let done = wait_job(&e, job["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done", "{done}");
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .contains("installing fake-acp@1.2.3")
            && done["output"]
                .as_str()
                .unwrap()
                .contains("ACP command ready")
    );
    let cat = a("GET", "/catalog".into(), None).await.1;
    let fa = cat["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "fake-acp")
        .unwrap()
        .clone();
    assert_eq!(
        (fa["installed"].as_bool(), fa["custom"].as_bool()),
        (Some(true), Some(true)),
        "{fa}"
    );

    // 探測與模型清單
    let p = a("POST", "/probe".into(), Some(json!({"agent": "fake-acp"})))
        .await
        .1;
    assert_eq!(
        (p["ok"].as_bool(), p["model"].as_str()),
        (Some(true), Some("m1")),
        "{p}"
    );
    let m = a("GET", "/models/fake-acp".into(), None).await.1;
    assert_eq!(m["models"].as_array().unwrap().len(), 3);
    assert_eq!(
        a("POST", "/probe".into(), Some(json!({"agent": "claude"})))
            .await
            .0,
        400,
        "not installed"
    );
    assert_eq!(
        a("POST", "/probe".into(), Some(json!({"agent": "nope"})))
            .await
            .0,
        404
    );

    // 新增角色（模型 m2），名稱檢查、與使用者同名、未安裝 agent
    assert_eq!(
        a(
            "POST",
            "/roles".into(),
            Some(json!({"role": "Bad Name", "agent": "fake-acp"}))
        )
        .await
        .0,
        400
    );
    assert_eq!(
        a(
            "POST",
            "/roles".into(),
            Some(json!({"role": "ann", "agent": "fake-acp"}))
        )
        .await
        .0,
        409
    );
    assert_eq!(
        a(
            "POST",
            "/roles".into(),
            Some(json!({"role": "x", "agent": "claude"}))
        )
        .await
        .0,
        400
    );
    let (st, r) = a("POST", "/roles".into(), Some(json!({"role": "coder", "agent": "fake-acp", "model": "m2", "rank": "lead", "label": "Coder"}))).await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["applied"]["added"], json!(["coder"]));
    assert_eq!(
        a(
            "POST",
            "/roles".into(),
            Some(json!({"role": "coder", "agent": "fake-acp"}))
        )
        .await
        .0,
        409
    );
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(e.home.path().join("deploy/roles.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        file["roles"]["coder"],
        json!({"driver": "acp", "acp_agent": "fake-acp", "model": "m2", "rank": "lead", "label": "Coder"})
    );

    // 真的對話：角色用 fake/m2
    let (st, rb) = call(
        &e.addr,
        "POST",
        "/api/v1/chat/rooms",
        Some(&e.owner),
        Some(json!({"name": "lab", "members": ["coder"]})),
    )
    .await;
    assert_eq!(st, 200, "{rb}");
    let room = rb["id"].as_i64().unwrap();
    let reply = |q: &'static str| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move {
            let before = call(
                &addr,
                "GET",
                &format!("/api/v1/chat/rooms/{room}/messages"),
                Some(&t),
                None,
            )
            .await
            .1
            .as_array()
            .unwrap()
            .len();
            call(
                &addr,
                "POST",
                &format!("/api/v1/chat/rooms/{room}/messages"),
                Some(&t),
                Some(json!({"body": format!("@coder {q}")})),
            )
            .await;
            for _ in 0..200 {
                let all = call(
                    &addr,
                    "GET",
                    &format!("/api/v1/chat/rooms/{room}/messages"),
                    Some(&t),
                    None,
                )
                .await
                .1;
                let all = all.as_array().unwrap();
                if let Some(m) = all[before..].iter().find(|m| m["author"] == "coder") {
                    return m["body"].as_str().unwrap().to_string();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("no reply")
        }
    };
    assert_eq!(reply("MODEL?").await, "MODEL: fake/m2");

    // 改模型：預設只驗連線（不送提示），叫不動的模型要 test=true 實測才抓得到；失敗就不改，成功才寫檔並套用
    let (st, b) = a(
        "POST",
        "/roles/coder/engine".into(),
        Some(json!({"model": "m-broken", "test": true})),
    )
    .await;
    assert_eq!(st, 422, "{b}");
    assert!(b["message"].as_str().unwrap().contains("model not found"));
    let (st, b) = a(
        "POST",
        "/roles/coder/engine".into(),
        Some(json!({"model": "nope"})),
    )
    .await;
    assert_eq!(st, 422);
    assert!(b["message"].as_str().unwrap().contains("no model nope"));
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(e.home.path().join("deploy/roles.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        file["roles"]["coder"]["model"], "m2",
        "unchanged after failed checks"
    );
    let (st, b) = a(
        "POST",
        "/roles/coder/engine".into(),
        Some(json!({"model": "m1"})),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    assert_eq!(b["check"]["ok"], true);
    assert_eq!(
        b["check"]["connect_only"], true,
        "default check sends no prompt"
    );
    assert!(b["check"].get("reply").is_none(), "{b}");
    assert_eq!(b["applied"]["changed"], json!(["coder"]));
    assert_eq!(reply("MODEL?").await, "MODEL: m1");

    // 推理強度：先驗 agent／模型有這個值才存（成功路徑在 runtime 的 acp_reasoning_effort_follows_the_model）；
    // 這個假 agent 不回報推理強度：擋下、不改檔
    let (st, b) = a(
        "POST",
        "/roles/coder/engine".into(),
        Some(json!({"effort": "high"})),
    )
    .await;
    assert_eq!(st, 422, "{b}");
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .contains("no reasoning effort setting"),
        "{b}"
    );
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(e.home.path().join("deploy/roles.json")).unwrap(),
    )
    .unwrap();
    assert!(file["roles"]["coder"].get("effort").is_none(), "unchanged");

    // headless 只給有非互動 CLI 的 agent（claude／codex／pi）；不認得的 transport 也擋
    let (st, b) = a(
        "POST",
        "/roles/coder/engine".into(),
        Some(json!({"transport": "headless"})),
    )
    .await;
    assert_eq!(st, 400, "{b}");
    assert!(b["message"]
        .as_str()
        .unwrap()
        .contains("only available for claude, codex and pi"));
    let (st, _) = a(
        "POST",
        "/roles/coder/engine".into(),
        Some(json!({"transport": "ssh"})),
    )
    .await;
    assert_eq!(st, 400);

    // health 與 dashboard（回合紀錄）
    let h = a("GET", "/health".into(), None).await.1;
    let c = &h["roles"][0];
    assert_eq!(
        (
            c["role"].as_str(),
            c["agent"].as_str(),
            c["model"].as_str(),
            c["last_turn"]["outcome"].as_str()
        ),
        (Some("coder"), Some("fake-acp"), Some("m1"), Some("ok")),
        "{h}"
    );
    assert!(c["agent_error"].is_null(), "{h}");
    let d = call(
        &e.addr,
        "GET",
        "/api/v1/agents/dashboard",
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(d["roles"], 1);
    assert_eq!(d["turns_24h"]["ok"]["count"], 2, "{d}");
    assert!(d["recent"].as_array().unwrap().len() >= 2);
    // ann 不是群成員：看不到該群的回合
    let d2 = call(
        &e.addr,
        "GET",
        "/api/v1/agents/dashboard",
        Some(&e.human),
        None,
    )
    .await
    .1;
    assert_eq!(d2["recent"].as_array().unwrap().len(), 0, "{d2}");

    // 舊設定仍指已移除的 gemini：卡片直接給原因（不靜默換 agent）
    let rp = e.home.path().join("deploy/roles.json");
    let mut file: Value = serde_json::from_str(&std::fs::read_to_string(&rp).unwrap()).unwrap();
    file["roles"]["old"] = json!({"driver": "acp", "acp_agent": "gemini", "rank": "worker"});
    // 全域可解析、只有某群覆寫成 gemini：該群那筆也要帶原因
    file["roles"]["legacy"] = json!({"driver": "acp", "acp_agent": "fake-acp", "rank": "worker",
        "room_overrides": {"17": {"acp_agent": "gemini"}, "18": {"model": "m1"}}});
    std::fs::write(&rp, file.to_string()).unwrap();
    let mut old = Value::Null;
    for _ in 0..100 {
        let h = a("GET", "/health".into(), None).await.1;
        if let Some(r) = h["roles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["role"] == "old")
        {
            old = r.clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        old["agent_error"]
            .as_str()
            .is_some_and(|s| s.contains("gemini was removed")),
        "{old}"
    );
    assert_eq!(old["agent"], "gemini", "agent is not silently swapped");
    let h = a("GET", "/health".into(), None).await.1;
    let legacy = h["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "legacy")
        .unwrap();
    assert!(legacy["agent_error"].is_null(), "{legacy}");
    let ovs = legacy["room_overrides"].as_array().unwrap();
    let by_room = |n: i64| ovs.iter().find(|o| o["room"] == n).unwrap()["agent_error"].clone();
    assert!(
        by_room(17)
            .as_str()
            .is_some_and(|s| s.contains("gemini was removed")),
        "{legacy}"
    );
    assert!(by_room(18).is_null(), "{legacy}");
    // 移除角色
    let (st, b) = a("DELETE", "/roles/coder".into(), None).await;
    assert_eq!(st, 200);
    assert_eq!(b["applied"]["removed"], json!(["coder"]));
    assert_eq!(a("DELETE", "/roles/coder".into(), None).await.0, 404);
}

#[tokio::test]
async fn install_one_at_a_time_and_failures() {
    let e = start().await;
    let post = |b: Value| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move { call(&addr, "POST", "/api/v1/agents/install", Some(&t), Some(b)).await }
    };
    // 內建 codex 走 npm 版本固定的套件
    let (st, j) = post(json!({"agent": "codex", "confirm": true})).await;
    assert_eq!(st, 200);
    assert_eq!(j["npm"], "@agentclientprotocol/codex-acp@2.2.2");
    let j = wait_job(&e, j["id"].as_str().unwrap()).await;
    assert_eq!(j["state"], "done", "{j}");
    // 失敗的安裝
    std::fs::write(e.home.path().join("vendor/acp/custom.json"), "[]").unwrap();
    let f = e.home.path().join("npm");
    let s = std::fs::read_to_string(&f)
        .unwrap()
        .replace("fake-acp*) echo", "fake-acp*|broken*) echo")
        .replace(
            "\"name\":\"fake-acp\",\"version\":\"1.2.3\"",
            "\"name\":\"broken\",\"version\":\"1.0.0\"",
        );
    std::fs::write(&f, s).unwrap();
    let (st, j) = post(json!({"npm": "broken", "confirm": true})).await;
    assert_eq!(st, 200, "{j}");
    let j = wait_job(&e, j["id"].as_str().unwrap()).await;
    assert_eq!(j["state"], "failed");
    assert!(j["output"].as_str().unwrap().contains("npm ERR! boom"));
    assert!(
        !std::fs::read_to_string(e.home.path().join("vendor/acp/custom.json"))
            .unwrap()
            .contains("broken"),
        "failed install not registered"
    );
    // 同時只能一個（slow 跑 3 秒）
    let s = std::fs::read_to_string(&f)
        .unwrap()
        .replace("fake-acp*|broken*) echo", "fake-acp*|broken*|slow*) echo")
        .replace("\"name\":\"broken\"", "\"name\":\"slow\"");
    std::fs::write(&f, s).unwrap();
    let (st, j) = post(json!({"npm": "slow", "confirm": true})).await;
    assert_eq!(st, 200, "{j}");
    let (st2, b) = post(json!({"agent": "codex", "confirm": true})).await;
    assert_eq!(st2, 409, "{b}");
    wait_job(&e, j["id"].as_str().unwrap()).await;
    // 不存在的 job
    assert_eq!(
        call(
            &e.addr,
            "GET",
            "/api/v1/agents/install/nope",
            Some(&e.owner),
            None
        )
        .await
        .0,
        404
    );
}

#[tokio::test]
async fn quota_lists_installed_agents_only() {
    let e = start().await;
    let (st, q) = call(&e.addr, "GET", "/api/v1/agents/quota", Some(&e.owner), None).await;
    assert_eq!(st, 200);
    // vendor 都沒裝：只可能有 hermes（看本機有沒有 hermes-acp）
    for x in q["accounts"].as_array().unwrap() {
        assert_eq!(x["agent"], "hermes", "{x}");
        assert!(x["windows"].is_array());
        let s = x.to_string();
        assert!(
            !s.contains("token") && !s.contains("gho_"),
            "no secrets: {s}"
        );
    }
    assert_eq!(
        call(&e.addr, "GET", "/api/v1/agents/quota", Some(&e.human), None)
            .await
            .0,
        403
    );
}

/// 每群覆寫：只有 owner 能改；疊加寫入 roles.json；只影響該群；YOLO 自動允許；
/// 系統公告不喚醒任何人；還原；群永久刪除時清掉覆寫。
#[tokio::test]
async fn per_room_override_model_mode_and_yolo() {
    let e = start().await;
    let a = |m: &'static str, p: String, b: Option<Value>| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move { call(&addr, m, &format!("/api/v1{p}"), Some(&t), b).await }
    };
    let job = a(
        "POST",
        "/agents/install".into(),
        Some(json!({"npm": "fake-acp", "confirm": true})),
    )
    .await
    .1;
    assert_eq!(
        wait_job(&e, job["id"].as_str().unwrap()).await["state"],
        "done"
    );
    // 模式清單：agent 回報的 + 最寬
    let m = a("GET", "/agents/models/fake-acp".into(), None).await.1;
    assert_eq!(m["modes"].as_array().map(|v| v.len()), Some(3), "{m}");
    assert_eq!(m["widest_mode"], "bypassPermissions");
    assert_eq!(
        a(
            "POST",
            "/agents/roles".into(),
            Some(json!({"role": "coder", "agent": "fake-acp", "model": "m1"}))
        )
        .await
        .0,
        200
    );
    let mk = |name: &'static str| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move {
            call(
                &addr,
                "POST",
                "/api/v1/chat/rooms",
                Some(&t),
                Some(json!({"name": name, "members": ["coder", "ann"]})),
            )
            .await
            .1["id"]
                .as_i64()
                .unwrap()
        }
    };
    let (r1, r2) = (mk("one").await, mk("two").await);
    let ask = |room: i64, q: &'static str| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move {
            let path = format!("/api/v1/chat/rooms/{room}/messages");
            let before = call(&addr, "GET", &path, Some(&t), None)
                .await
                .1
                .as_array()
                .unwrap()
                .len();
            call(
                &addr,
                "POST",
                &path,
                Some(&t),
                Some(json!({"body": format!("@coder {q}")})),
            )
            .await;
            for _ in 0..300 {
                let all = call(&addr, "GET", &path, Some(&t), None).await.1;
                if let Some(m) = all.as_array().unwrap()[before..]
                    .iter()
                    .find(|m| m["author"] == "coder")
                {
                    return m["body"].as_str().unwrap().to_string();
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            panic!("no reply in room {room}")
        }
    };
    assert_eq!(ask(r1, "MODEL?").await, "MODEL: m1");
    assert_eq!(ask(r2, "MODEL?").await, "MODEL: m1");
    let path = format!("/agents/rooms/{r2}/roles/coder");
    // 非 owner：可看不可改
    let (st, v) = call(
        &e.addr,
        "GET",
        &format!("/api/v1{path}"),
        Some(&e.human),
        None,
    )
    .await;
    assert_eq!((st, v["can_edit"].as_bool()), (200, Some(false)), "{v}");
    assert_eq!(
        call(
            &e.addr,
            "PUT",
            &format!("/api/v1{path}"),
            Some(&e.human),
            Some(json!({"model": "fake/m2"}))
        )
        .await
        .0,
        403
    );
    // 模型實測失敗：不改（test=true 才實測；預設只驗連線、清單裡有 m-broken 就過）
    let (st, b) = a(
        "PUT",
        path.clone(),
        Some(json!({"model": "m-broken", "test": true})),
    )
    .await;
    assert_eq!(st, 422, "{b}");
    let (st, b) = a("PUT", path.clone(), Some(json!({"model": "nope"}))).await;
    assert_eq!(st, 422, "{b}");
    // 群 2：換模型＋YOLO（期限遠在未來）
    let (st, b) = a(
        "PUT",
        path.clone(),
        Some(json!({"model": "fake/m2", "yolo": true, "yolo_until": "2099-12-31"})),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(e.home.path().join("deploy/roles.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        file["roles"]["coder"]["room_overrides"][r2.to_string()],
        json!({"model": "fake/m2", "yolo": true, "yolo_until": "2099-12-31"})
    );
    assert_eq!(file["roles"]["coder"]["model"], "m1", "global untouched");
    let v = a("GET", path.clone(), None).await.1;
    assert_eq!(
        (v["effective"]["model"].as_str(), v["yolo"].as_bool()),
        (Some("fake/m2"), Some(true)),
        "{v}"
    );
    // 只影響群 2
    assert_eq!(ask(r2, "MODEL?").await, "MODEL: fake/m2");
    assert_eq!(ask(r2, "MODE?").await, "MODE: bypassPermissions");
    assert_eq!(ask(r1, "MODEL?").await, "MODEL: m1");
    assert_eq!(ask(r1, "MODE?").await, "MODE: default");
    // YOLO：權限請求自動允許，不跳卡片
    assert_eq!(ask(r2, "NEED_PERMISSION").await, "PERMISSION: allow");
    // 系統公告：一則、不喚醒任何人
    let msgs = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r2}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    let sys: Vec<&Value> = msgs
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["author_kind"] == "system"
                && m["body"]
                    .as_str()
                    .unwrap()
                    .contains("set coder in this room")
        })
        .collect();
    assert_eq!(sys.len(), 1, "{msgs}");
    assert!(sys[0]["body"]
        .as_str()
        .unwrap()
        .contains("YOLO until 2099-12-31"));
    // 只改模式（不開 YOLO）；UI 會送空字串欄位
    let (st, _) = a("PUT", path.clone(), Some(json!({"agent": "", "model": "", "mode": "acceptEdits", "yolo": false, "yolo_until": ""}))).await;
    assert_eq!(st, 200);
    assert_eq!(ask(r2, "MODE?").await, "MODE: acceptEdits");
    assert_eq!(
        ask(r2, "MODEL?").await,
        "MODEL: m1",
        "model override cleared by the new layer"
    );
    // 壞日期
    assert_eq!(
        a(
            "PUT",
            path.clone(),
            Some(json!({"yolo": true, "yolo_until": "next week"}))
        )
        .await
        .0,
        400
    );
    // 不在群裡的角色
    assert_eq!(
        a(
            "PUT",
            format!("/agents/rooms/{r2}/roles/ghost"),
            Some(json!({"model": "m1"}))
        )
        .await
        .0,
        400
    );
    // 還原
    let (st, b) = a("DELETE", path.clone(), None).await;
    assert_eq!((st, b["removed"].as_bool()), (200, Some(true)));
    assert_eq!(ask(r2, "MODE?").await, "MODE: default");
    // health 顯示覆寫數
    a("PUT", path.clone(), Some(json!({"yolo": true}))).await;
    let h = a("GET", "/agents/health".into(), None).await.1;
    assert_eq!(
        h["roles"][0]["room_overrides"],
        json!([{"room": r2, "yolo": true, "agent_error": null}]),
        "{h}"
    );
    // 群永久刪除：覆寫跟著清掉
    a(
        "POST",
        format!("/chat/rooms/{r2}/state"),
        Some(json!({"action": "delete"})),
    )
    .await;
    let (st, b) = a(
        "POST",
        format!("/chat/rooms/{r2}/purge"),
        Some(json!({"confirm_name": "two"})),
    )
    .await;
    assert!(st == 200 || st == 204, "{st} {b}");
    let mut gone = false;
    for _ in 0..50 {
        let f: Value = serde_json::from_str(
            &std::fs::read_to_string(e.home.path().join("deploy/roles.json")).unwrap(),
        )
        .unwrap();
        if f["roles"]["coder"].get("room_overrides").is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "override removed after purge");
}

/// 套件清單、檢查更新、升級（暫存目錄先實測，失敗保留舊版；成功移除改名前的舊套件，
/// 用到的 lane 回合結束後以新程式重開）。
#[tokio::test]
async fn packages_list_check_and_tested_upgrade() {
    let e = start().await;
    let a = |m: &'static str, p: &'static str, b: Option<Value>| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move { call(&addr, m, &format!("/api/v1/agents{p}"), Some(&t), b).await }
    };
    let pkg_of = |v: &Value, id: &str| {
        v["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["agent"] == id)
            .cloned()
            .unwrap()
    };
    // 只有 owner
    assert_eq!(
        call(
            &e.addr,
            "GET",
            "/api/v1/agents/packages",
            Some(&e.human),
            None
        )
        .await
        .0,
        403
    );
    // 清單：所有 agent（hermes 走 PATH，沒有套件欄）；vendor 類都還沒裝
    let (st, l) = a("GET", "/packages", None).await;
    assert_eq!(st, 200, "{l}");
    let ids: Vec<&str> = l["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["agent"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["hermes", "claude", "codex", "pi", "agy"]);
    let cx = pkg_of(&l, "codex");
    assert_eq!(cx["npm"], "@agentclientprotocol/codex-acp");
    assert_eq!(cx["pinned"], "2.2.2");
    assert_eq!(cx["installed"], Value::Null);
    assert_eq!(cx["latest"], Value::Null, "not checked yet");
    // 舊套件（改名前）裝著：列在 legacy
    let old = e
        .home
        .path()
        .join("vendor/acp/node_modules/@zed-industries/codex-acp");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("package.json"), r#"{"version":"0.16.0"}"#).unwrap();
    // 檢查更新（npm view）
    let (st, l) = a("POST", "/packages/check", None).await;
    assert_eq!(st, 200, "{l}");
    let cx = pkg_of(&l, "codex");
    assert_eq!(cx["latest"], "9.9.9", "{cx}");
    assert_eq!(cx["legacy"][0]["version"], "0.16.0");
    assert!(cx["checked_at"].as_u64().is_some());
    // 沒 confirm／壞版本字串／hermes 不歸我們管
    assert_eq!(
        a("POST", "/packages/upgrade", Some(json!({"agent": "codex"})))
            .await
            .0,
        400
    );
    assert_eq!(
        a(
            "POST",
            "/packages/upgrade",
            Some(json!({"agent": "codex", "version": "1 --foo", "confirm": true}))
        )
        .await
        .0,
        400
    );
    assert_eq!(
        a(
            "POST",
            "/packages/upgrade",
            Some(json!({"agent": "hermes", "confirm": true}))
        )
        .await
        .0,
        400
    );
    // vendor 安裝失敗：已移除的舊套件要裝回去
    let (_, j) = a(
        "POST",
        "/packages/upgrade",
        Some(json!({"agent": "codex", "version": "0.0.0-vendorfail", "confirm": true})),
    )
    .await;
    let j = wait_job(&e, j["id"].as_str().unwrap()).await;
    let out = j["output"].as_str().unwrap_or_default().to_string();
    assert_eq!(j["state"], "failed", "{out}");
    assert!(
        out.contains("putting back @zed-industries/codex-acp@0.16.0"),
        "{out}"
    );
    assert!(old.join("package.json").exists(), "legacy restored: {out}");
    // 第一次：裝建議版本（暫存→實測→vendor），舊套件移除
    let (st, j) = a(
        "POST",
        "/packages/upgrade",
        Some(json!({"agent": "codex", "confirm": true})),
    )
    .await;
    assert_eq!(st, 200, "{j}");
    assert_eq!(j["kind"], "upgrade");
    let j = wait_job(&e, j["id"].as_str().unwrap()).await;
    let out = j["output"].as_str().unwrap_or_default().to_string();
    assert_eq!(j["state"], "done", "{out}");
    assert!(out.contains("test ok"), "{out}");
    assert!(out.contains("removing @zed-industries/codex-acp"), "{out}");
    assert!(!old.exists(), "legacy package removed");
    let l = a("GET", "/packages", None).await.1;
    assert_eq!(pkg_of(&l, "codex")["installed"], "2.2.2");
    assert_eq!(pkg_of(&l, "codex")["legacy"], json!([]));
    assert!(
        !e.home
            .path()
            .join("var/acp-stage")
            .read_dir()
            .unwrap()
            .any(|_| true),
        "staging folder cleaned"
    );

    // 有角色在用：開一條 lane，記下 session
    assert_eq!(
        a(
            "POST",
            "/roles",
            Some(json!({"role": "coder", "agent": "codex"}))
        )
        .await
        .0,
        200
    );
    let (_, rb) = call(
        &e.addr,
        "POST",
        "/api/v1/chat/rooms",
        Some(&e.owner),
        Some(json!({"name": "lab", "members": ["coder"]})),
    )
    .await;
    let room = rb["id"].as_i64().unwrap();
    let ask = || {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move {
            let path = format!("/api/v1/chat/rooms/{room}/messages");
            let before = call(&addr, "GET", &path, Some(&t), None)
                .await
                .1
                .as_array()
                .unwrap()
                .len();
            call(
                &addr,
                "POST",
                &path,
                Some(&t),
                Some(json!({"body": "@coder WHOAMI"})),
            )
            .await;
            for _ in 0..200 {
                let all = call(&addr, "GET", &path, Some(&t), None).await.1;
                if let Some(m) = all.as_array().unwrap()[before..]
                    .iter()
                    .find(|m| m["author"] == "coder")
                {
                    return m["body"].as_str().unwrap().to_string();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("no reply")
        }
    };
    let s1 = ask().await;
    assert!(s1.ends_with("turn 1"), "{s1}");
    assert_eq!(
        ask().await.split(' ').next(),
        s1.split(' ').next(),
        "same session"
    );

    // 壞版本：實測失敗，什麼都不變（版本、session）
    let (st, j) = a(
        "POST",
        "/packages/upgrade",
        Some(json!({"agent": "codex", "version": "0.0.0-bad", "confirm": true})),
    )
    .await;
    assert_eq!(st, 200, "{j}");
    let j = wait_job(&e, j["id"].as_str().unwrap()).await;
    let out = j["output"].as_str().unwrap_or_default().to_string();
    assert_eq!(j["state"], "failed", "{out}");
    assert!(
        out.contains("test failed") && out.contains("nothing changed"),
        "{out}"
    );
    assert_eq!(
        pkg_of(&a("GET", "/packages", None).await.1, "codex")["installed"],
        "2.2.2"
    );
    let s3 = ask().await;
    assert_eq!(s3.split(' ').next(), s1.split(' ').next(), "{s3}");

    // 升級到 9.9.9：成功；用到的 lane 重開，下一回合是新 session
    let (st, j) = a(
        "POST",
        "/packages/upgrade",
        Some(json!({"agent": "codex", "version": "9.9.9", "confirm": true})),
    )
    .await;
    assert_eq!(st, 200, "{j}");
    let j = wait_job(&e, j["id"].as_str().unwrap()).await;
    let out = j["output"].as_str().unwrap_or_default().to_string();
    assert_eq!(j["state"], "done", "{out}");
    assert!(out.contains("1 open session(s) restart"), "{out}");
    assert_eq!(
        pkg_of(&a("GET", "/packages", None).await.1, "codex")["installed"],
        "9.9.9"
    );
    let s4 = ask().await;
    assert_ne!(
        s4.split(' ').next(),
        s1.split(' ').next(),
        "new session: {s4}"
    );
    assert!(s4.ends_with("turn 1"), "{s4}");
    assert_eq!(
        pkg_of(&a("GET", "/packages", None).await.1, "codex")["used_by"],
        json!(["coder"])
    );
}

/// 本機本體（cli）版本、官方最新版、相容紀錄、實測後才替換本機（npm 全域 prefix）。
#[tokio::test]
async fn engine_versions_compat_and_tested_engine_upgrade() {
    let e = start().await;
    let a = |m: &'static str, p: &'static str, b: Option<Value>| {
        let (addr, t) = (e.addr.clone(), e.owner.clone());
        async move { call(&addr, m, &format!("/api/v1/agents{p}"), Some(&t), b).await }
    };
    let h = e.home.path();
    // 本機「全域安裝」的 fakecli 1.0.0：<g>/lib/node_modules/fakecli/cli.sh，<g>/bin/fakecli 是連結
    let g = h.join("g");
    let pkgdir = g.join("lib/node_modules/fakecli");
    std::fs::create_dir_all(&pkgdir).unwrap();
    std::fs::write(pkgdir.join("cli.sh"), "#!/bin/sh\necho \"fakecli 1.0.0\"\n").unwrap();
    std::fs::set_permissions(
        pkgdir.join("cli.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::create_dir_all(g.join("bin")).unwrap();
    std::os::unix::fs::symlink(pkgdir.join("cli.sh"), g.join("bin/fakecli")).unwrap();
    let cli = g.join("bin/fakecli").to_string_lossy().to_string();
    // 先裝轉接器（自訂 fake-acp），再在 custom.json 標上本體
    let j = a(
        "POST",
        "/install",
        Some(json!({"npm": "fake-acp", "confirm": true})),
    )
    .await
    .1;
    assert_eq!(
        wait_job(&e, j["id"].as_str().unwrap()).await["state"],
        "done"
    );
    let cf = h.join("vendor/acp/custom.json");
    let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&cf).unwrap()).unwrap();
    c[0]["engine"] = json!({"source": "cli", "version_cmd": [cli, "--version"], "env": "FAKE_ENGINE_BIN", "latest": "npm:fakecli"});
    std::fs::write(&cf, c.to_string()).unwrap();
    let pk = |v: &Value| {
        v["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["agent"] == "fake-acp")
            .cloned()
            .unwrap()
    };
    let l = a("GET", "/packages", None).await.1;
    let f = pk(&l);
    assert_eq!(f["engine"]["version"], "1.0.0", "{f}");
    assert_eq!(f["engine"]["managed"], true);
    assert_eq!(f["compat"]["state"], "unknown");
    // 檢查更新：官方最新版 2.0.0
    let f = pk(&a("POST", "/packages/check", None).await.1);
    assert_eq!(f["engine"]["latest"], "2.0.0", "{f}");
    // 手動測試相容：通過並記錄
    let (st, t) = a("POST", "/packages/test", Some(json!({"agent": "fake-acp"}))).await;
    assert_eq!((st, t["ok"].as_bool()), (200, Some(true)), "{t}");
    let f = pk(&a("GET", "/packages", None).await.1);
    assert_eq!(f["compat"]["state"], "ok", "{f}");
    assert_eq!(f["compat"]["engine"], "1.0.0");
    // 本體 bad 版：實測失敗，本機不動，紀錄留下失敗（是另一組版本，目前這組仍是 ok）
    let j = a(
        "POST",
        "/packages/engine-upgrade",
        Some(json!({"agent": "fake-acp", "version": "0.0.0-bad", "confirm": true})),
    )
    .await;
    assert_eq!(j.0, 200, "{}", j.1);
    let j = wait_job(&e, j.1["id"].as_str().unwrap()).await;
    let out = j["output"].as_str().unwrap_or_default().to_string();
    assert_eq!(j["state"], "failed", "{out}");
    assert!(
        out.contains("test failed") && out.contains("nothing changed"),
        "{out}"
    );
    assert!(std::fs::read_to_string(pkgdir.join("cli.sh"))
        .unwrap()
        .contains("1.0.0"));
    let f = pk(&a("GET", "/packages", None).await.1);
    assert_eq!(
        (
            f["engine"]["version"].as_str(),
            f["compat"]["state"].as_str()
        ),
        (Some("1.0.0"), Some("ok"))
    );
    // 2.0.0：通過 → 用本機的 prefix 全域安裝 → 版本 2.0.0、相容 ok
    let j = a(
        "POST",
        "/packages/engine-upgrade",
        Some(json!({"agent": "fake-acp", "version": "2.0.0", "confirm": true})),
    )
    .await
    .1;
    let j = wait_job(&e, j["id"].as_str().unwrap()).await;
    let out = j["output"].as_str().unwrap_or_default().to_string();
    assert_eq!(j["state"], "done", "{out}");
    assert!(
        out.contains("--prefix") && out.contains("installed version now: 2.0.0"),
        "{out}"
    );
    let f = pk(&a("GET", "/packages", None).await.1);
    assert_eq!(
        (
            f["engine"]["version"].as_str(),
            f["compat"]["state"].as_str()
        ),
        (Some("2.0.0"), Some("ok")),
        "{f}"
    );
    // 壞版本字串／非 owner
    assert_eq!(
        a(
            "POST",
            "/packages/engine-upgrade",
            Some(json!({"agent": "fake-acp", "version": "1;rm", "confirm": true}))
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(
            &e.addr,
            "POST",
            "/api/v1/agents/packages/test",
            Some(&e.human),
            Some(json!({"agent": "fake-acp"}))
        )
        .await
        .0,
        403
    );
}
