//! chat feature 端到端：真實 HTTP＋核心背景工作（dispatcher job）＋假 ACP agent。

use aaf_api::{AuthFeature, HttpFeature};
use aaf_core::feature::BootOptions;
use aaf_core::storage::Db;
use aaf_core::{Core, Registry};
use aaf_feature_chat::Chat;
use aaf_runtime::dispatcher::{RoleSpec, Roster};
use aaf_runtime::DriverSpec;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 測試二進位自己扮演假 ACP agent：以 AAF_FAKE_ACP=1 重新執行本測試檔。
fn fake_acp_spec() -> DriverSpec {
    DriverSpec::Acp {
        command: vec![
            std::env::current_exe().unwrap().to_string_lossy().into(),
            "--exact".into(),
            "fake_agent_entry".into(),
            "--nocapture".into(),
        ],
        acp_agent: None,
        model: None,
        mode: None,
        cwd: None,
        env: vec![("AAF_FAKE_ACP".into(), "1".into())],
        effort: None,
        account: None,
        transport: None,
    }
}

#[test]
fn fake_agent_entry() {
    if std::env::var("AAF_FAKE_ACP").is_ok() {
        aaf_runtime::testing::fake_acp_main();
        std::process::exit(0);
    }
}

struct Wrap(Arc<Chat>);
impl aaf_core::Feature for Wrap {
    fn name(&self) -> &'static str {
        aaf_core::Feature::name(&*self.0)
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
    fn jobs(&self) -> Vec<aaf_core::jobs::Job> {
        self.0.jobs()
    }
}

struct Env {
    addr: String,
    core: Arc<Core>,
    owner: String,
    ann: String,
    chat: Arc<Chat>,
    _home: tempfile::TempDir,
}

async fn start(retention_days: &str) -> Env {
    start_with(retention_days, |_| {}).await
}

async fn start_with(retention_days: &str, tweak: impl FnOnce(&mut Roster)) -> Env {
    start_env(retention_days, &[], tweak).await
}

async fn start_env(
    retention_days: &str,
    extra: &[(&str, &str)],
    tweak: impl FnOnce(&mut Roster),
) -> Env {
    let tmp = std::env::temp_dir();
    let home = tempfile::tempdir().unwrap();
    let mut roster = Roster {
        instance: home.path().to_path_buf(),
        ..Default::default()
    };
    roster
        .roles
        .insert("coder".into(), RoleSpec::new(fake_acp_spec(), &tmp));
    roster
        .roles
        .insert("reviewer".into(), RoleSpec::new(fake_acp_spec(), &tmp));
    roster.roles.insert(
        "echo".into(),
        RoleSpec::new(
            DriverSpec::Command {
                command: vec!["/usr/bin/tail".into(), "-n1".into()],
                cwd: None,
            },
            &tmp,
        ),
    );
    tweak(&mut roster);
    let chat = Arc::new(Chat::with_tick(roster, Duration::from_millis(50)));
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Wrap(chat.clone()))).unwrap();
    let opts = BootOptions {
        env: vec![(
            "AAF_CHAT_TRASH_RETENTION_DAYS".into(),
            retention_days.into(),
        )]
        .into_iter()
        .chain(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .collect(),
        ..Default::default()
    };
    let core = Core::boot(&reg, Db::open_in_memory().unwrap(), opts).unwrap();
    let http: Vec<Arc<dyn HttpFeature>> = vec![chat.clone()];
    let app = aaf_api::app(core.clone(), &http);
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
    Env {
        addr,
        core,
        owner,
        ann,
        chat,
        _home: home,
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
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len()).as_bytes())
        .await
        .unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let json = body
        .find(['{', '['])
        .and_then(|i| {
            serde_json::Deserializer::from_str(&body[i..])
                .into_iter::<Value>()
                .next()?
                .ok()
        })
        .unwrap_or_default();
    (status, json)
}

async fn wait_messages(e: &Env, room: i64, n: usize) -> Vec<Value> {
    for _ in 0..200 {
        let m = call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{room}/messages"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if m.as_array().unwrap().len() >= n {
            return m.as_array().unwrap().clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("room {room} did not reach {n} messages");
}

async fn room(e: &Env, name: &str, members: &[&str], token: &str) -> i64 {
    let (st, r) = call(
        &e.addr,
        "POST",
        "/api/v1/chat/rooms",
        Some(token),
        Some(json!({"name": name, "members": members})),
    )
    .await;
    assert_eq!(st, 200, "{r}");
    r["id"].as_i64().unwrap()
}

#[tokio::test]
async fn message_wakes_role_and_reply_lands_in_same_room_thread() {
    let e = start("30").await;
    let r1 = room(&e, "build", &["coder", "ann"], &e.owner).await;
    let r2 = room(&e, "review", &["coder"], &e.owner).await;
    let (st, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r1}/messages"),
        Some(&e.owner),
        Some(json!({"body": "WHOAMI"})),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(m["delivered_to"], json!(["coder"]));
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r2}/messages"),
        Some(&e.owner),
        Some(json!({"body": "WHOAMI"})),
    )
    .await;

    let a = wait_messages(&e, r1, 2).await;
    let b = wait_messages(&e, r2, 2).await;
    assert_eq!(a[1]["author"], "coder");
    assert_eq!(a[1]["reply_to"], a[0]["id"]);
    assert_eq!(a[1]["thread_id"], a[0]["id"]);
    let sid = |m: &Value| {
        m["body"]
            .as_str()
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string()
    };
    assert_ne!(sid(&a[1]), sid(&b[1]), "each room has its own session");

    // 同一群第二輪：同一個 session
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r1}/messages"),
        Some(&e.ann),
        Some(json!({"body": "WHOAMI"})),
    )
    .await;
    let a = wait_messages(&e, r1, 4).await;
    assert_eq!(a[3]["body"], format!("{} turn 2", sid(&a[1])));

    // 角色的回覆不會再喚醒任何人（沒有 @提及）：穩定在 4 則
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(wait_messages(&e, r1, 4).await.len(), 4);

    let lanes = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r1}/lanes"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(
        lanes["lanes"],
        json!([{"role": "coder", "status": "idle", "pending_permissions": [], "stuck": null}])
    );
}

#[tokio::test]
async fn mentions_target_roles_and_role_messages_only_reach_mentioned() {
    let e = start("30").await;
    let r = room(&e, "team", &["coder", "echo", "reviewer"], &e.owner).await;
    let (_, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@echo hi there"})),
    )
    .await;
    assert_eq!(m["delivered_to"], json!(["echo"]));
    let msgs = wait_messages(&e, r, 2).await;
    assert_eq!(msgs[1]["author"], "echo");
    // echo 的回覆內容含 "@echo hi there"（tail 回原文最後一行）→ 不含其他角色 → 不喚醒任何人
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(wait_messages(&e, r, 2).await.len(), 2);

    // 無提及：給群內所有可喚醒角色
    let (_, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "all hands"})),
    )
    .await;
    let mut to: Vec<String> = serde_json::from_value(m["delivered_to"].clone()).unwrap();
    to.sort();
    assert_eq!(to, vec!["coder", "echo", "reviewer"]);
    wait_messages(&e, r, 6).await;
}

#[tokio::test]
async fn membership_isolation_and_permissions() {
    let e = start("30").await;
    let private = room(&e, "owner only", &["coder"], &e.owner).await;
    // ann 不是成員：看不到、不能發
    assert_eq!(
        call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{private}"),
            Some(&e.ann),
            None
        )
        .await
        .0,
        404
    );
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{private}/messages"),
            Some(&e.ann),
            Some(json!({"body": "x"}))
        )
        .await
        .0,
        404
    );
    let ann_rooms = call(&e.addr, "GET", "/api/v1/chat/rooms", Some(&e.ann), None)
        .await
        .1;
    assert!(ann_rooms.as_array().unwrap().is_empty());
    // ann 可建群（human 有 chat.room.create），但不能刪群（Owner 限定）
    let mine = room(&e, "ann's", &[], &e.ann).await;
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{mine}/state"),
            Some(&e.ann),
            Some(json!({"action": "delete"}))
        )
        .await
        .0,
        403
    );
    // 未知成員、跨群 reply_to 都被拒
    assert_eq!(
        call(
            &e.addr,
            "POST",
            "/api/v1/chat/rooms",
            Some(&e.owner),
            Some(json!({"name": "x", "members": ["ghost"]}))
        )
        .await
        .0,
        400
    );
    let (_, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{private}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@nobody x"})),
    )
    .await;
    let pid = m["message"]["id"].as_i64().unwrap();
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{mine}/messages"),
            Some(&e.ann),
            Some(json!({"body": "x", "reply_to": pid}))
        )
        .await
        .0,
        400
    );
}

#[tokio::test]
async fn remove_member_cancels_queue_clears_lead_and_keeps_history() {
    let e = start("30").await;
    let r = room(&e, "crew", &["ann", "coder", "reviewer"], &e.owner).await;
    assert_eq!(set_lead(&e, r, Some("coder"), &e.owner).await.0, 200);
    // 凍結讓給 reviewer 的信留在佇列，才看得到移除時有沒有取消
    let path = format!("/api/v1/chat/rooms/{r}");
    let frozen = json!({"frozen": true, "reason": "hold"});
    assert_eq!(
        call(&e.addr, "PATCH", &path, Some(&e.owner), Some(frozen))
            .await
            .0,
        200
    );
    let (_, m) = call(
        &e.addr,
        "POST",
        &format!("{path}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@reviewer hi"})),
    )
    .await;
    let mid = m["message"]["id"].as_i64().unwrap();
    let remove = |who: &str, token: &str| {
        let (addr, p, t) = (
            e.addr.clone(),
            format!("{path}/members/{who}"),
            token.to_string(),
        );
        async move { call(&addr, "DELETE", &p, Some(&t), None).await }
    };

    assert_eq!(
        remove("alice", &e.owner).await.0,
        409,
        "cannot remove yourself"
    );
    let (st, v) = remove("reviewer", &e.owner).await;
    assert_eq!((st, &v["removed"]), (200, &json!(true)));
    let state: String = e
        .core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT state FROM chat_deliveries WHERE message = ?1 AND recipient = 'reviewer'",
                [mid],
                |x| x.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(state, "cancelled");
    assert_eq!(
        remove("reviewer", &e.owner).await.1["removed"],
        json!(false),
        "already gone"
    );

    assert_eq!(remove("coder", &e.owner).await.0, 200);
    assert_eq!(remove("ann", &e.owner).await.0, 200);
    let (_, info) = call(&e.addr, "GET", &path, Some(&e.owner), None).await;
    let left: Vec<&str> = info["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["member"].as_str().unwrap())
        .collect();
    assert_eq!(left, vec!["alice"]);
    assert_eq!(info["lead"], Value::Null, "removing the lead clears it");
    assert_eq!(
        call(&e.addr, "GET", &path, Some(&e.ann), None).await.0,
        404,
        "removed user loses access"
    );
    // 歷史保留，並有系統公告
    let msgs = wait_messages(&e, r, 1).await;
    assert!(msgs.iter().any(|m| m["id"] == mid));
    assert!(msgs
        .iter()
        .any(|m| m["body"] == "alice removed coder from the room."));
}

#[tokio::test]
async fn agents_stop_waking_each_other_after_an_agent_only_chain() {
    // ping 和 pong 都回提示最後一行：最後一行含 @ping @pong，兩邊會互相叫醒停不下來
    let echo = |roster: &mut Roster| {
        for r in ["ping", "pong"] {
            roster.roles.insert(
                r.into(),
                RoleSpec::new(
                    DriverSpec::Command {
                        command: vec!["/usr/bin/tail".into(), "-n1".into()],
                        cwd: None,
                    },
                    std::env::temp_dir(),
                ),
            );
        }
    };
    let e = start_with("30", echo).await;
    let r = room(&e, "loop", &["ping", "pong"], &e.owner).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@ping @pong say hi to each other"})),
    )
    .await;
    let notice = |m: &Vec<Value>| {
        m.iter().any(|x| {
            x["author_kind"] == "system"
                && x["body"].as_str().unwrap_or("").contains("Agents paused")
        })
    };
    let mut all = vec![];
    for _ in 0..400 {
        all = wait_messages(&e, r, 1).await;
        if notice(&all) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(notice(&all), "guard notice never posted: {all:#?}");
    // 公告之後不再有新的叫醒：已在跑或已排隊的回合（每個角色最多一則）跑完就停
    tokio::time::sleep(Duration::from_millis(800)).await;
    let roles = |m: &Vec<Value>| m.iter().filter(|x| x["author_kind"] == "role").count();
    let settled = roles(&wait_messages(&e, r, 1).await);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = wait_messages(&e, r, 1).await;
    assert_eq!(roles(&after), settled, "the loop stopped: {after:#?}");
    assert!(
        settled <= 8 + 2,
        "at most one in-flight reply per role after the limit: {settled}"
    );
    assert_eq!(
        after
            .iter()
            .filter(|x| x["author_kind"] == "system")
            .count(),
        1,
        "one notice only"
    );
    // 有人發言就恢復
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@ping once more"})),
    )
    .await;
    let mut resumed = false;
    for _ in 0..200 {
        if roles(&wait_messages(&e, r, 1).await) > settled {
            resumed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(resumed, "a human message resumes waking");
}

#[tokio::test]
async fn agent_chain_setting_is_owner_only_and_overrides_config() {
    // 只寫回預設值 8：上限是行程全域的，改成別的值會干擾同時在跑的其他測試
    let e = start("30").await;
    let get = |tok: String| {
        let addr = e.addr.clone();
        async move { call(&addr, "GET", "/api/v1/chat/settings", Some(&tok), None).await }
    };
    let put = |tok: String, b: Value| {
        let addr = e.addr.clone();
        async move { call(&addr, "PUT", "/api/v1/chat/settings", Some(&tok), Some(b)).await }
    };
    let (st, s) = get(e.owner.clone()).await;
    assert_eq!(st, 200, "{s}");
    assert_eq!(
        (s["agent_chain"].as_u64(), s["agent_chain_source"].as_str()),
        (Some(8), Some("config"))
    );
    assert_eq!(get(e.ann.clone()).await.0, 403);
    assert_eq!(put(e.ann.clone(), json!({"agent_chain": 8})).await.0, 403);
    let (st, b) = put(e.owner.clone(), json!({"agent_chain": 101})).await;
    assert_eq!(st, 400, "{b}");
    let (st, s) = put(e.owner.clone(), json!({"agent_chain": 8})).await;
    assert_eq!(
        (st, s["agent_chain_source"].as_str()),
        (200, Some("page")),
        "{s}"
    );
    let saved: String = e
        .core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT value FROM chat_settings WHERE key='agent_chain'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(saved, "8");
    let (st, s) = put(e.owner.clone(), json!({"agent_chain": null})).await;
    assert_eq!(
        (st, s["agent_chain_source"].as_str()),
        (200, Some("config")),
        "{s}"
    );
}

#[tokio::test]
async fn freeze_stops_waking_until_unfrozen() {
    let e = start("30").await;
    let r = room(&e, "brake", &["echo"], &e.owner).await;
    assert_eq!(
        call(
            &e.addr,
            "PATCH",
            &format!("/api/v1/chat/rooms/{r}"),
            Some(&e.owner),
            Some(json!({"frozen": true}))
        )
        .await
        .0,
        400,
        "reason required"
    );
    assert_eq!(
        call(
            &e.addr,
            "PATCH",
            &format!("/api/v1/chat/rooms/{r}"),
            Some(&e.owner),
            Some(json!({"frozen": true, "reason": "runaway"}))
        )
        .await
        .0,
        200
    );
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "while frozen"})),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let all = wait_messages(&e, r, 2).await;
    assert!(
        all.iter().all(|m| m["author_kind"] != "role"),
        "no reply while frozen"
    );
    // 凍結會發系統公告（含理由），不喚醒角色
    assert_eq!(all[0]["author_kind"], "system");
    assert!(
        all[0]["body"]
            .as_str()
            .unwrap()
            .contains("Room frozen by alice")
            && all[0]["body"].as_str().unwrap().contains("runaway")
    );
    call(
        &e.addr,
        "PATCH",
        &format!("/api/v1/chat/rooms/{r}"),
        Some(&e.owner),
        Some(json!({"frozen": false, "reason": "fixed"})),
    )
    .await;
    let m = wait_messages(&e, r, 4).await;
    assert_eq!(m[2]["author_kind"], "system");
    let reply = m.iter().find(|x| x["author_kind"] == "role").unwrap();
    assert_eq!(reply["body"], "while frozen");
    let audit = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/audit"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    let actions: Vec<&str> = audit
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, vec!["created", "frozen", "unfrozen"]);
}

#[tokio::test]
async fn trash_restore_purge_and_audit_survives() {
    let e = start("30").await;
    let r = room(&e, "doomed", &["coder"], &e.owner).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "hello"})),
    )
    .await;
    wait_messages(&e, r, 2).await;

    // 不在垃圾桶不能永久刪
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/purge"),
            Some(&e.owner),
            Some(json!({"confirm_name": "doomed"}))
        )
        .await
        .0,
        409
    );
    // 刪除 → 進垃圾桶；成員看不到、Owner 在垃圾桶看得到；不能再發言
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/state"),
            Some(&e.owner),
            Some(json!({"action": "delete"}))
        )
        .await
        .0,
        200
    );
    let trash = call(
        &e.addr,
        "GET",
        "/api/v1/chat/rooms?state=deleted",
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(trash[0]["id"], r);
    assert_eq!(trash[0]["deleted_by"], "alice");
    assert_eq!(
        call(
            &e.addr,
            "GET",
            "/api/v1/chat/rooms?state=deleted",
            Some(&e.ann),
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
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&e.owner),
            Some(json!({"body": "x"}))
        )
        .await
        .0,
        400
    );
    // 恢復再刪
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/state"),
            Some(&e.owner),
            Some(json!({"action": "restore"}))
        )
        .await
        .0,
        200
    );
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/state"),
        Some(&e.owner),
        Some(json!({"action": "delete"})),
    )
    .await;
    // 名稱確認錯誤 → 400
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/purge"),
            Some(&e.owner),
            Some(json!({"confirm_name": "nope"}))
        )
        .await
        .0,
        400
    );
    let (st, _) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/purge"),
        Some(&e.owner),
        Some(json!({"confirm_name": "doomed"})),
    )
    .await;
    assert_eq!(st, 200);

    // 資料清掉、lane 關閉、session 遺忘；稽核仍在
    let counts: (i64, i64, i64) = e
        .core
        .db
        .with(|c| {
            Ok((
                c.query_row(
                    "SELECT count(*) FROM chat_messages WHERE room = ?1",
                    [r],
                    |x| x.get(0),
                )?,
                c.query_row(
                    "SELECT count(*) FROM chat_members WHERE room = ?1",
                    [r],
                    |x| x.get(0),
                )?,
                c.query_row(
                    "SELECT count(*) FROM chat_sessions WHERE room = ?1",
                    [r],
                    |x| x.get(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!(counts, (0, 0, 0));
    assert_eq!(
        call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}"),
            Some(&e.owner),
            None
        )
        .await
        .0,
        404
    );
    let audit = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/audit"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    let actions: Vec<&str> = audit
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        vec!["created", "delete", "restore", "delete", "purged"]
    );
    assert_eq!(audit[4]["detail"], "2 messages");
    assert!(e
        .core
        .bus
        .since(0, 1000)
        .unwrap()
        .iter()
        .any(|ev| ev.kind == "chat.room.purged"));
}

#[tokio::test]
async fn purge_refused_while_turn_running_then_allowed_after_cancel() {
    let e = start("30").await;
    let r = room(&e, "slow", &["coder"], &e.owner).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "SLOW"})),
    )
    .await;
    for _ in 0..200 {
        let l = call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/lanes"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if l["lanes"][0]["status"] == "working" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 回合進行中：封存成功（取消待投遞），但永久刪除被拒
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/state"),
        Some(&e.owner),
        Some(json!({"action": "delete"})),
    )
    .await;
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/purge"),
            Some(&e.owner),
            Some(json!({"confirm_name": "slow"}))
        )
        .await
        .0,
        409
    );
    // 停止按鈕
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/lanes/coder/cancel"),
            Some(&e.owner),
            None
        )
        .await
        .1["cancelled"],
        true
    );
    let mut ok = false;
    for _ in 0..200 {
        if call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/purge"),
            Some(&e.owner),
            Some(json!({"confirm_name": "slow"})),
        )
        .await
        .0 == 200
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "purge allowed after the turn ended");
}

#[tokio::test]
async fn permission_request_answered_over_http() {
    let e = start("30").await;
    let r = room(&e, "perm", &["coder"], &e.owner).await;
    let mut sub = e.core.bus.subscribe();
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "NEED_PERMISSION"})),
    )
    .await;
    let req = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ev = sub.next().await.unwrap();
            if ev.kind == "runtime.permission.requested" {
                return ev;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(req.payload["lane"], json!({"role": "coder", "room": r}));
    let rid = req.payload["event"]["request_id"].as_u64().unwrap();
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/lanes/coder/permission"),
            Some(&e.ann),
            Some(json!({"request_id": rid, "option_id": "allow"}))
        )
        .await
        .0,
        404,
        "ann not a member"
    );
    let (st, _) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/lanes/coder/permission"),
        Some(&e.owner),
        Some(json!({"request_id": rid, "option_id": "allow"})),
    )
    .await;
    assert_eq!(st, 200);
    let m = wait_messages(&e, r, 2).await;
    assert_eq!(m[1]["body"], "PERMISSION: allow");
}

#[tokio::test]
async fn trash_retention_job_purges_expired_rooms() {
    let e = start("1").await;
    let r = room(&e, "old", &[], &e.owner).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/state"),
        Some(&e.owner),
        Some(json!({"action": "delete"})),
    )
    .await;
    // 把刪除時間往前調兩天，直接呼叫清理（job 每小時跑一次，這裡不等）
    e.core
        .db
        .with(|c| {
            Ok(c.execute(
                "UPDATE chat_rooms SET deleted_at = deleted_at - 2*86400 WHERE id = ?1",
                [r],
            )?)
        })
        .unwrap();
    let disp = aaf_runtime::dispatcher::Dispatcher::new(
        Arc::new(aaf_feature_chat::inbox::ChatInbox {
            db: e.core.db.clone(),
            bus: e.core.bus.clone(),
            roster: aaf_runtime::dispatcher::shared_roster(Roster::default()),
            hooks: None,
        }),
        e.core.bus.clone(),
        Roster::default(),
    );
    assert_eq!(
        aaf_feature_chat::store::purge_expired(&e.core, &disp)
            .await
            .unwrap(),
        vec![r]
    );
    let audit = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/audit"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(
        audit.as_array().unwrap().last().unwrap()["actor"],
        "system:retention"
    );
}

/// 讀 SSE 直到收到符合條件的事件或逾時；回傳收到的全部事件 kind。
async fn sse_kinds(addr: &str, token: &str, ms: u64) -> Vec<Value> {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET /api/v1/events?after=0 HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nAccept: text/event-stream\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = String::new();
    let mut chunk = [0u8; 8192];
    let _ = tokio::time::timeout(Duration::from_millis(ms), async {
        loop {
            let k = s.read(&mut chunk).await.unwrap();
            if k == 0 {
                break;
            }
            buf.push_str(&String::from_utf8_lossy(&chunk[..k]));
        }
    })
    .await;
    buf.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[tokio::test]
async fn sse_hides_other_rooms_from_non_members() {
    let e = start("30").await;
    let secret = room(&e, "owner secret", &["echo"], &e.owner).await;
    let shared = room(&e, "shared", &["ann"], &e.owner).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{secret}/messages"),
        Some(&e.owner),
        Some(json!({"body": "classified"})),
    )
    .await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{shared}/messages"),
        Some(&e.owner),
        Some(json!({"body": "hello ann"})),
    )
    .await;
    wait_messages(&e, secret, 2).await;
    let ann = sse_kinds(&e.addr, &e.ann, 500).await;
    let text = serde_json::to_string(&ann).unwrap();
    assert!(text.contains("hello ann"), "ann sees her room");
    assert!(
        !text.contains("classified"),
        "ann must not see other rooms' messages: {text}"
    );
    assert!(!text.contains("owner secret"));
    assert!(
        !ann.iter()
            .any(|e| e["kind"].as_str().unwrap().starts_with("runtime.")),
        "no runtime events from rooms ann is not in"
    );
    let owner = serde_json::to_string(&sse_kinds(&e.addr, &e.owner, 500).await).unwrap();
    assert!(owner.contains("classified") && owner.contains("runtime.turn.started"));
}

#[tokio::test]
async fn unread_counts_mentions_and_read_cursor() {
    let e = start("30").await;
    let r = room(&e, "u", &["ann"], &e.owner).await;
    for b in ["one", "two @ann", "three"] {
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&e.owner),
            Some(json!({"body": b})),
        )
        .await;
    }
    let u = call(&e.addr, "GET", "/api/v1/chat/unread", Some(&e.ann), None)
        .await
        .1;
    assert_eq!(u, json!([{"room": r, "unread": 3, "mentions": 1}]));
    assert!(
        call(&e.addr, "GET", "/api/v1/chat/unread", Some(&e.owner), None)
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty(),
        "own messages are not unread"
    );
    let ids: Vec<i64> = wait_messages(&e, r, 3)
        .await
        .iter()
        .map(|m| m["id"].as_i64().unwrap())
        .collect();
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/read"),
        Some(&e.ann),
        Some(json!({"through": ids[1]})),
    )
    .await;
    assert_eq!(
        call(&e.addr, "GET", "/api/v1/chat/unread", Some(&e.ann), None)
            .await
            .1,
        json!([{"room": r, "unread": 1, "mentions": 0}])
    );
    // 游標不會倒退
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/read"),
        Some(&e.ann),
        Some(json!({"through": ids[0]})),
    )
    .await;
    assert_eq!(
        call(&e.addr, "GET", "/api/v1/chat/unread", Some(&e.ann), None)
            .await
            .1[0]["unread"],
        1
    );
}

#[tokio::test]
async fn pending_permission_visible_in_lanes_after_reload() {
    let e = start("30").await;
    let r = room(&e, "p", &["coder"], &e.owner).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "NEED_PERMISSION"})),
    )
    .await;
    let mut pending = Value::Null;
    for _ in 0..200 {
        let l = call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/lanes"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if l["lanes"][0]["pending_permissions"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            pending = l["lanes"][0].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(pending["status"], "blocked");
    let p = &pending["pending_permissions"][0];
    assert_eq!(p["title"], "write file");
    let rid = p["request_id"].as_u64().unwrap();
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/lanes/coder/permission"),
        Some(&e.owner),
        Some(json!({"request_id": rid, "option_id": "deny"})),
    )
    .await;
    assert_eq!(wait_messages(&e, r, 2).await[1]["body"], "PERMISSION: deny");
    let l = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/lanes"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert!(l["lanes"][0]["pending_permissions"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn roles_json_hot_reload_from_disk() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("deploy")).unwrap();
    let file = home.path().join("deploy/roles.json");
    std::fs::write(
        &file,
        r#"{"roles": {"echo": {"driver": "command", "command": ["/usr/bin/tail", "-n1"]}}}"#,
    )
    .unwrap();
    let roster = Roster::load(home.path()).unwrap();
    let chat = Arc::new(Chat::with_tick(roster, Duration::from_millis(50)));
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Wrap(chat.clone()))).unwrap();
    let core = Core::boot(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    let http: Vec<Arc<dyn HttpFeature>> = vec![chat.clone()];
    let app = aaf_api::app(core.clone(), &http);
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
    let roles = |a: String, t: String| async move {
        call(&a, "GET", "/api/v1/chat/roles", Some(&t), None)
            .await
            .1
    };
    assert_eq!(
        roles(addr.clone(), t.clone())
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // 讓 watcher 先記錄初始狀態
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = chat.shared().reload(&core).await;

    // 加一個角色：手動 reload API
    std::fs::write(
        &file,
        r#"{"roles": {"echo": {"driver": "command", "command": ["/usr/bin/tail", "-n1"]}, "upper": {"driver": "command", "command": ["/usr/bin/tr", "a-z", "A-Z"]}}}"#,
    )
    .unwrap();
    let (st, r) = call(&addr, "POST", "/api/v1/chat/roles/reload", Some(&t), None).await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["diff"]["added"], json!(["upper"]));
    let room_id = call(
        &addr,
        "POST",
        "/api/v1/chat/rooms",
        Some(&t),
        Some(json!({"name": "hot", "members": ["upper"]})),
    )
    .await
    .1["id"]
        .as_i64()
        .unwrap();
    call(
        &addr,
        "POST",
        &format!("/api/v1/chat/rooms/{room_id}/messages"),
        Some(&t),
        Some(json!({"body": "@upper shout"})),
    )
    .await;
    let mut got = String::new();
    for _ in 0..200 {
        let m = call(
            &addr,
            "GET",
            &format!("/api/v1/chat/rooms/{room_id}/messages"),
            Some(&t),
            None,
        )
        .await
        .1;
        if let Some(x) = m.as_array().unwrap().get(1) {
            got = x["body"].as_str().unwrap().to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        got.contains("@UPPER SHOUT"),
        "new role works without restart: {got}"
    );

    // 壞掉的 JSON：保留舊設定、回 400、發 runtime.roster.invalid
    std::fs::write(&file, "{ not json").unwrap();
    assert_eq!(
        call(&addr, "POST", "/api/v1/chat/roles/reload", Some(&t), None)
            .await
            .0,
        400
    );
    assert_eq!(
        roles(addr.clone(), t.clone())
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(core
        .bus
        .since(0, 5000)
        .unwrap()
        .iter()
        .any(|e| e.kind == "runtime.roster.invalid"));

    // 背景 watcher：寫入有效檔案後 2 秒內自動生效（移除 upper）
    std::fs::write(
        &file,
        r#"{"roles": {"echo": {"driver": "command", "command": ["/usr/bin/tail", "-n1"]}}}"#,
    )
    .unwrap();
    let mut ok = false;
    for _ in 0..100 {
        if roles(addr.clone(), t.clone())
            .await
            .as_array()
            .unwrap()
            .len()
            == 1
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "watcher applied roles.json change");
    // 非 Owner 不可 reload
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
        call(&addr, "POST", "/api/v1/chat/roles/reload", Some(&ann), None)
            .await
            .0,
        403
    );
}

/// 取角色 token：以 Chat 內部的發 token 流程（與 agent 啟動時相同），再讀回記憶體中的值不可行 —
/// 測試直接重新發一個（舊的隨之失效），模擬 agent 拿到的 AAF_TOKEN。
fn role_token(core: &Core, role: &str, rank: &str) -> String {
    aaf_api::issue_role_token(core, role, rank).unwrap()
}

#[tokio::test]
async fn task_lifecycle_with_role_tokens() {
    let e = start("30").await;
    let r = room(&e, "work", &["coder", "reviewer"], &e.owner).await;
    let coder = role_token(&e.core, "coder", "worker");
    let reviewer = role_token(&e.core, "reviewer", "worker");
    let lead = role_token(&e.core, "echo", "lead"); // echo 不在群內

    // 角色身分
    assert_eq!(
        call(&e.addr, "GET", "/api/v1/chat/whoami", Some(&coder), None)
            .await
            .1,
        json!({"name": "coder", "rank": "worker"})
    );
    assert_eq!(
        call(
            &e.addr,
            "GET",
            "/api/v1/chat/whoami",
            Some("aafr_bogus"),
            None
        )
        .await
        .0,
        401
    );

    // worker 不能派任務；指派對象必須是群成員；Owner 派給 coder
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/tasks"),
            Some(&coder),
            Some(json!({"title": "x"}))
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/tasks"),
            Some(&e.owner),
            Some(json!({"title": "x", "assignee": "echo"}))
        )
        .await
        .0,
        409
    );
    let (st, t) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks"),
        Some(&e.owner),
        Some(json!({"title": "Write README", "spec": "keep it short", "assignee": "coder"})),
    )
    .await;
    assert_eq!(st, 200, "{t}");
    let id = t["id"].as_i64().unwrap();
    assert_eq!(t["state"], "open");
    // 任務訊息貼在群裡、@coder 喚醒 coder（假 agent 會回 ECHO）
    let msgs = wait_messages(&e, r, 2).await;
    assert!(msgs[0]["body"]
        .as_str()
        .unwrap()
        .starts_with(&format!("@coder [task #{id}] Write README")));
    assert_eq!(msgs[1]["author"], "coder");

    // 不在群內的 lead 看不到這個任務
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/tasks/{id}/claim"),
            Some(&lead),
            None
        )
        .await
        .0,
        404
    );
    // 未認領不能回報
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/tasks/{id}/report"),
            Some(&coder),
            Some(json!({"state": "done"}))
        )
        .await
        .0,
        403
    );
    // coder 認領；reviewer 不能再認領
    let (st, t) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{id}/claim"),
        Some(&coder),
        None,
    )
    .await;
    assert_eq!(
        (st, t["state"].as_str(), t["assignee"].as_str()),
        (200, Some("claimed"), Some("coder"))
    );
    assert!(t["lease_until"].as_i64().unwrap() > 0);
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/tasks/{id}/claim"),
            Some(&reviewer),
            None
        )
        .await
        .0,
        409
    );
    // 受阻 → 發起人接手 → 完成
    let (st, t) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{id}/report"),
        Some(&coder),
        Some(json!({"state": "blocked", "result": "need access"})),
    )
    .await;
    assert_eq!((st, t["state"].as_str()), (200, Some("blocked")));
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/tasks/{id}/claim"),
            Some(&reviewer),
            None
        )
        .await
        .0,
        403,
        "blocked: only assignee/owner"
    );
    let (_, t) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{id}/claim"),
        Some(&e.owner),
        None,
    )
    .await;
    assert_eq!(t["assignee"], "alice", "owner took over");
    let (_, t) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{id}/report"),
        Some(&e.owner),
        Some(json!({"state": "done", "result": "README written"})),
    )
    .await;
    assert_eq!(t["state"], "done");
    assert_eq!(t["result"], "README written");
    // 群裡有完整紀錄：blocked 回報 @alice、接手通知 @coder、done
    let bodies: Vec<String> = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1
    .as_array()
    .unwrap()
    .iter()
    .map(|m| m["body"].as_str().unwrap().to_string())
    .collect();
    assert!(
        bodies
            .iter()
            .any(|b| b.starts_with(&format!("@alice [task #{id} → blocked] need access"))),
        "{bodies:?}"
    );
    assert!(bodies
        .iter()
        .any(|b| b.starts_with(&format!("@coder [task #{id}] taken over by alice"))));
    assert!(bodies
        .iter()
        .any(|b| b == &format!("[task #{id} → done] README written")));
    // 已完成不可取消；列表
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/tasks/{id}/report"),
            Some(&e.owner),
            Some(json!({"state": "cancelled"}))
        )
        .await
        .0,
        409
    );
    let list = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/tasks"),
        Some(&coder),
        None,
    )
    .await
    .1;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(
        call(&e.addr, "GET", "/api/v1/chat/tasks", Some(&lead), None)
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty(),
        "not visible outside the room"
    );
}

/// 09：@ 到 roles.json 有、但不在本群的角色 → 系統通知「沒有送達」，作者是角色時 @作者；10 分鐘內不重貼。
#[tokio::test]
async fn mention_outside_room_posts_undelivered_notice() {
    let e = start("30").await;
    let r = room(&e, "solo", &["coder"], &e.owner).await;
    let coder = role_token(&e.core, "coder", "worker");
    let notices = |msgs: &[Value]| -> Vec<String> {
        msgs.iter()
            .filter(|m| {
                m["author_kind"] == "system" && m["body"].as_str().unwrap().contains("不在本群")
            })
            .map(|m| m["body"].as_str().unwrap().to_string())
            .collect()
    };
    let list = |e: &Env| {
        let (a, o) = (e.addr.clone(), e.owner.clone());
        async move {
            call(
                &a,
                "GET",
                &format!("/api/v1/chat/rooms/{r}/messages"),
                Some(&o),
                None,
            )
            .await
            .1
            .as_array()
            .unwrap()
            .clone()
        }
    };
    // 人：@ 群外角色＋不認得的名字＋email → 只通知群外角色，不帶 @
    let (_, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@reviewer @nobody mail a@b.com"})),
    )
    .await;
    assert!(!m["delivered_to"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x == "reviewer"));
    let n = notices(&list(&e).await);
    assert_eq!(
        n,
        vec!["你提到的 reviewer 不在本群，這則沒有送給他們。".to_string()]
    );

    // 角色：@ 群外角色 → 通知帶 @作者並投遞給作者
    let (st, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&coder),
        Some(json!({"body": "@reviewer @echo please check"})),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(m["delivered_to"], json!([]));
    let msgs = list(&e).await;
    let n = notices(&msgs);
    assert_eq!(n.len(), 2, "{n:?}");
    assert!(
        n[1].starts_with("@coder 你提到的 reviewer、echo 不在本群"),
        "{}",
        n[1]
    );
    let nid = msgs
        .iter()
        .find(|m| m["body"].as_str() == Some(n[1].as_str()))
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let delivered: Vec<String> = e
        .core
        .db
        .with(|c| {
            let mut st = c.prepare("SELECT recipient FROM chat_deliveries WHERE message = ?1")?;
            let v = st
                .query_map([nid], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
        .unwrap();
    assert_eq!(delivered, vec!["coder".to_string()]);

    // 10 分鐘內同樣內容不重貼
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&coder),
        Some(json!({"body": "@reviewer @echo again"})),
    )
    .await;
    assert_eq!(notices(&list(&e).await).len(), 2);
}

#[tokio::test]
async fn role_can_post_and_only_wakes_mentioned() {
    let e = start("30").await;
    let r = room(&e, "talk", &["coder", "reviewer"], &e.owner).await;
    let coder = role_token(&e.core, "coder", "worker");
    let (st, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&coder),
        Some(json!({"body": "@reviewer please check"})),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(m["message"]["author_kind"], "role");
    assert_eq!(m["delivered_to"], json!(["reviewer"]));
    let (_, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&coder),
        Some(json!({"body": "status: working"})),
    )
    .await;
    assert_eq!(
        m["delivered_to"],
        json!([]),
        "role messages without mentions wake nobody"
    );
    // 角色不能做 Owner 的事
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/state"),
            Some(&coder),
            Some(json!({"action": "delete"}))
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &e.addr,
            "POST",
            "/api/v1/chat/rooms",
            Some(&coder),
            Some(json!({"name": "mine"}))
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &e.addr,
            "POST",
            "/api/v1/auth/users",
            Some(&coder),
            Some(json!({"name": "x", "password": "password123"}))
        )
        .await
        .0,
        403
    );
}

#[tokio::test]
async fn expired_lease_returns_task_to_open() {
    let e = start("30").await;
    let r = room(&e, "lease", &["reviewer"], &e.owner).await;
    let rev = role_token(&e.core, "reviewer", "worker");
    let id = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks"),
        Some(&e.owner),
        Some(json!({"title": "t"})),
    )
    .await
    .1["id"]
        .as_i64()
        .unwrap();
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{id}/claim"),
        Some(&rev),
        None,
    )
    .await;
    e.core
        .db
        .with(|c| Ok(c.execute("UPDATE chat_tasks SET lease_until = 1 WHERE id = ?1", [id])?))
        .unwrap();
    let list = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/tasks"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(
        (list[0]["state"].as_str(), list[0]["assignee"].is_null()),
        (Some("open"), true)
    );
    assert!(e
        .core
        .bus
        .since(0, 5000)
        .unwrap()
        .iter()
        .any(|ev| ev.kind == "chat.task.updated" && ev.payload["reason"] == "lease_expired"));
}

#[tokio::test]
async fn agents_receive_role_token_and_room_in_env() {
    let tmp = std::env::temp_dir();
    let mut roster = Roster::default();
    let mut rs = RoleSpec::new(
        DriverSpec::Command {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo \"$AAF_ROLE|$AAF_ROOM|$AAF_URL|${AAF_TOKEN%%_*}\"".into(),
            ],
            cwd: None,
        },
        &tmp,
    );
    rs.rank = "lead".into();
    roster.roles.insert("probe".into(), rs);
    let chat = Arc::new(Chat::with_tick(roster, Duration::from_millis(50)));
    chat.set_api_url("http://127.0.0.1:9");
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Wrap(chat.clone()))).unwrap();
    let core = Core::boot(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    let http: Vec<Arc<dyn HttpFeature>> = vec![chat.clone()];
    let app = aaf_api::app(core.clone(), &http);
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
    let r = call(
        &addr,
        "POST",
        "/api/v1/chat/rooms",
        Some(&t),
        Some(json!({"name": "env", "members": ["probe"]})),
    )
    .await
    .1["id"]
        .as_i64()
        .unwrap();
    call(
        &addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&t),
        Some(json!({"body": "go"})),
    )
    .await;
    let mut got = String::new();
    for _ in 0..200 {
        let m = call(
            &addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&t),
            None,
        )
        .await
        .1;
        if let Some(x) = m.as_array().unwrap().get(1) {
            got = x["body"].as_str().unwrap().to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(got, format!("probe|{r}|http://127.0.0.1:9|aafr"));
    let rank: String = core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT rank FROM auth_role_tokens WHERE role = 'probe'",
                [],
                |x| x.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(rank, "lead");
}

#[tokio::test]
async fn assigned_task_is_reserved_and_notices_do_not_wake_everyone() {
    let e = start("30").await;
    let r = room(&e, "assign", &["coder", "reviewer"], &e.owner).await;
    let reviewer = role_token(&e.core, "reviewer", "worker");
    let (_, t) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks"),
        Some(&e.owner),
        Some(json!({"title": "only coder", "assignee": "coder"})),
    )
    .await;
    let id = t["id"].as_i64().unwrap();
    assert_eq!(t["assignee"], "coder", "assignment is recorded at creation");
    assert_eq!(t["state"], "open");
    // 指派給 coder：reviewer 不能搶
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/tasks/{id}/claim"),
            Some(&reviewer),
            None
        )
        .await
        .0,
        403
    );
    // 建立訊息只喚醒 coder（@coder），reviewer 沒有投遞
    let deliv: Vec<String> = e.core.db.with(|c| {
        let mut st = c.prepare("SELECT d.recipient FROM chat_deliveries d JOIN chat_tasks t ON t.message = d.message WHERE t.id = ?1")?;
        let v = st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }).unwrap();
    assert_eq!(deliv, vec!["coder".to_string()]);
    // 人（發起人）接手、回報：通知不喚醒任何角色
    let (_, t) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{id}/claim"),
        Some(&e.owner),
        None,
    )
    .await;
    assert_eq!(t["assignee"], "alice");
    let before: i64 = e
        .core
        .db
        .with(|c| Ok(c.query_row("SELECT count(*) FROM chat_deliveries", [], |r| r.get(0))?))
        .unwrap();
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{id}/report"),
        Some(&e.owner),
        Some(json!({"state": "done", "result": "ok"})),
    )
    .await;
    let after: i64 = e
        .core
        .db
        .with(|c| Ok(c.query_row("SELECT count(*) FROM chat_deliveries", [], |r| r.get(0))?))
        .unwrap();
    // 接手通知 @coder 投遞 1 筆；done 通知（發起人自己回報，無 @）0 筆
    assert_eq!(after - before, 0, "owner's own done report wakes nobody");
}

#[tokio::test]
async fn task_events_reach_members_only() {
    let e = start("30").await;
    let r = room(&e, "private", &["coder"], &e.owner).await;
    call(
        &e.addr,
        "POST",
        "/api/v1/auth/users",
        Some(&e.owner),
        Some(json!({"name": "ann", "password": "password123"})),
    )
    .await;
    let ann = call(
        &e.addr,
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
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks"),
        Some(&e.owner),
        Some(json!({"title": "secret"})),
    )
    .await;
    let evs: Vec<String> = sse_kinds(&e.addr, &ann, 400)
        .await
        .iter()
        .map(|v| v["kind"].as_str().unwrap_or_default().to_string())
        .collect();
    let owner_evs: Vec<String> = sse_kinds(&e.addr, &e.owner, 400)
        .await
        .iter()
        .map(|v| v["kind"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        owner_evs.iter().any(|k| k == "chat.task.created"),
        "sanity: owner sees it: {owner_evs:?}"
    );
    assert!(
        !evs.iter().any(|k| k.starts_with("chat.task.")),
        "non-member must not see task events: {evs:?}"
    );
    assert_eq!(
        call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/tasks"),
            Some(&ann),
            None
        )
        .await
        .0,
        404
    );
}

/// 逐字稿：每日一檔＋_index；冪等；凍結／封存匯出、垃圾桶不匯出；
/// 時區分日；永久刪除預設保留、可選一併刪除。
#[tokio::test]
async fn transcript_export() {
    let e = start("30").await;
    let out = tempfile::tempdir().unwrap();
    let a = room(&e, "alpha", &[], &e.owner).await;
    let b = room(&e, "beta", &[], &e.owner).await;
    let c = room(&e, "gamma", &[], &e.owner).await;
    for (r, body) in [
        (a, "hello"),
        (a, "second line\nmultiline"),
        (b, "in beta"),
        (c, "in gamma"),
    ] {
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&e.owner),
            Some(json!({"body": body})),
        )
        .await;
    }
    // 回覆
    let first = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{a}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1[0]["id"]
        .as_i64()
        .unwrap();
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{a}/messages"),
        Some(&e.owner),
        Some(json!({"body": "reply", "reply_to": first})),
    )
    .await;
    // 兩天：把第一則改到 2026-01-01 23:30 台北（UTC 15:30）
    e.core
        .db
        .with(|c| {
            Ok(c.execute(
                "UPDATE chat_messages SET created_at = 1767281400 WHERE id = ?1",
                [first],
            )?)
        })
        .unwrap();
    // beta 凍結、gamma 進垃圾桶
    call(
        &e.addr,
        "PATCH",
        &format!("/api/v1/chat/rooms/{b}"),
        Some(&e.owner),
        Some(json!({"frozen": true, "reason": "t"})),
    )
    .await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{c}/state"),
        Some(&e.owner),
        Some(json!({"action": "delete"})),
    )
    .await;

    let opts = |since: Option<&'static str>| aaf_feature_chat::transcript::Options {
        root: out.path(),
        tz: Some("Asia/Taipei"),
        rooms: None,
        since,
    };
    let rep = aaf_feature_chat::transcript::export(&e.core, &opts(None)).unwrap();
    assert_eq!(rep.rooms, 2, "trash room is not exported");
    let dir = out.path().join(a.to_string());
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|f| f.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(files.len(), 3, "{files:?}"); // 2026-01-01.md, <today>.md, _index.md
    assert_eq!(files[0], "2026-01-01.md");
    let d1 = std::fs::read_to_string(dir.join("2026-01-01.md")).unwrap();
    assert!(d1.starts_with(&format!("# 群 {a}「alpha」逐字稿 · 2026-01-01")));
    assert!(d1.contains("Asia/Taipei（UTC+08:00）"));
    assert!(
        d1.contains(&format!("### #{first} · 23:30:00 · alice\nhello")),
        "{d1}"
    );
    let today = std::fs::read_to_string(dir.join(&files[1])).unwrap();
    assert!(today.contains("second line\nmultiline"));
    assert!(today.contains(&format!("· alice ↩ #{first}\nreply")));
    let idx = std::fs::read_to_string(dir.join("_index.md")).unwrap();
    assert!(idx.contains(&format!("| 2026-01-01 | 1 | #{first}–#{first} |")));
    assert!(
        std::fs::read_to_string(out.path().join(b.to_string()).join(&files[1]))
            .unwrap()
            .contains("- 狀態：已凍結")
    );
    assert!(!out.path().join(c.to_string()).exists());

    // 冪等：沒變就不寫（mtime 不動）
    let mt = std::fs::metadata(dir.join("2026-01-01.md"))
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert!(aaf_feature_chat::transcript::export(&e.core, &opts(None))
        .unwrap()
        .written
        .is_empty());
    assert_eq!(
        std::fs::metadata(dir.join("2026-01-01.md"))
            .unwrap()
            .modified()
            .unwrap(),
        mt
    );
    // 新訊息只改當天檔與索引
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{a}/messages"),
        Some(&e.owner),
        Some(json!({"body": "more"})),
    )
    .await;
    let w = aaf_feature_chat::transcript::export(&e.core, &opts(None))
        .unwrap()
        .written;
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(w.iter().any(|p| p.ends_with(&files[1])) && w.iter().any(|p| p.ends_with("_index.md")));
    // since：不碰較早的檔與索引
    std::fs::remove_file(dir.join("2026-01-01.md")).unwrap();
    aaf_feature_chat::transcript::export(&e.core, &opts(Some("2026-06-01"))).unwrap();
    assert!(!dir.join("2026-01-01.md").exists());
}

#[tokio::test]
async fn transcript_api_and_purge_options() {
    let e = start("30").await;
    let r1 = room(&e, "keepme", &[], &e.owner).await;
    let r2 = room(&e, "dropme", &[], &e.owner).await;
    for r in [r1, r2] {
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&e.owner),
            Some(json!({"body": "x"})),
        )
        .await;
    }
    // 預設輸出：<instance>/outputs/transcripts
    let (st, rep) = call(
        &e.addr,
        "POST",
        "/api/v1/chat/transcripts/export",
        Some(&e.owner),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, 200, "{rep}");
    assert_eq!(rep["rooms"], 2);
    let root = e.chat.shared().transcript_root(&e.core);
    assert!(root.join(r1.to_string()).join("_index.md").exists());
    assert_eq!(
        call(
            &e.addr,
            "POST",
            "/api/v1/chat/transcripts/export",
            Some(&e.owner),
            Some(json!({"since": "nope"}))
        )
        .await
        .0,
        400
    );
    // 非 Owner 不可匯出
    assert_eq!(
        call(
            &e.addr,
            "POST",
            "/api/v1/chat/transcripts/export",
            Some(&e.ann),
            Some(json!({}))
        )
        .await
        .0,
        403
    );
    // 永久刪除：預設保留
    for (r, name, del) in [(r1, "keepme", false), (r2, "dropme", true)] {
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/state"),
            Some(&e.owner),
            Some(json!({"action": "delete"})),
        )
        .await;
        let (st, b) = call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/purge"),
            Some(&e.owner),
            Some(json!({"confirm_name": name, "delete_transcripts": del})),
        )
        .await;
        assert_eq!(st, 200, "{b}");
        assert_eq!(b["transcripts_deleted"], del);
    }
    assert!(root.join(r1.to_string()).exists(), "kept by default");
    assert!(!root.join(r2.to_string()).exists(), "deleted when asked");
}

/// 原始位元組上傳（測試用）：回傳 (狀態碼, JSON, 原始 headers)。
async fn raw_req(
    addr: &str,
    method: &str,
    path: &str,
    token: &str,
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", body.len());
    s.write_all(head.as_bytes()).await.unwrap();
    // 伺服器可能依 Content-Length 提早拒絕並關閉連線：寫入失敗不算錯
    let _ = s.write_all(body).await;
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..split]).to_string();
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let mut body = out[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut dec = Vec::new();
        let mut rest = &body[..];
        loop {
            let nl = rest.windows(2).position(|w| w == b"\r\n").unwrap();
            let n = usize::from_str_radix(std::str::from_utf8(&rest[..nl]).unwrap().trim(), 16)
                .unwrap();
            if n == 0 {
                break;
            }
            dec.extend_from_slice(&rest[nl + 2..nl + 2 + n]);
            rest = &rest[nl + 2 + n + 2..];
        }
        body = dec;
    }
    (status, head, body)
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 7]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

#[tokio::test]
async fn attachments_upload_send_download() {
    let e = start("30").await;
    let r = room(&e, "files", &["coder"], &e.owner).await;
    let other = room(&e, "other", &[], &e.owner).await;
    let up = |name: &str| format!("/api/v1/chat/rooms/{r}/attachments?name={name}");

    // 檔案：任意位元組（含 HTML）一律當下載
    let doc = b"<html><script>alert(1)</script></html>".to_vec();
    let (st, _, b) = raw_req(&e.addr, "POST", &up("report%20v1.html"), &e.owner, &doc).await;
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
    let file: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (
            file["kind"].as_str(),
            file["name"].as_str(),
            file["size"].as_i64()
        ),
        (Some("file"), Some("report v1.html"), Some(doc.len() as i64))
    );
    assert!(file.get("sha256").is_none(), "digest/path not exposed");

    // 圖片：PNG 尾巴夾帶資料 → 重新編碼後消失
    let mut img = png(40, 30);
    img.extend_from_slice(b"TRAILING-SECRET");
    let (st, _, b) = raw_req(&e.addr, "POST", &up("photo.jpg"), &e.owner, &img).await;
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
    let pic: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (
            pic["kind"].as_str(),
            pic["name"].as_str(),
            pic["width"].as_i64(),
            pic["height"].as_i64()
        ),
        (Some("image"), Some("photo.png"), Some(40), Some(30))
    );

    // 不合法：壞圖片、隱藏檔、路徑、空檔、非成員、非 active
    assert_eq!(
        raw_req(&e.addr, "POST", &up("x.png"), &e.owner, b"not an image")
            .await
            .0,
        400
    );
    assert_eq!(
        raw_req(&e.addr, "POST", &up(".env"), &e.owner, b"A=1")
            .await
            .0,
        400
    );
    let (st, _, b) = raw_req(
        &e.addr,
        "POST",
        &up("..%2F..%2Fetc%2Fpasswd"),
        &e.owner,
        b"x",
    )
    .await;
    assert_eq!(st, 200, "path is reduced to the last segment");
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap()["name"],
        "passwd"
    );
    assert_eq!(
        raw_req(&e.addr, "POST", &up("empty.txt"), &e.owner, b"")
            .await
            .0,
        400
    );
    assert_eq!(
        raw_req(&e.addr, "POST", &up("a.txt"), &e.ann, b"x").await.0,
        404,
        "non-member"
    );

    // 發訊息綁定附件；只有附件也可以
    let ids = json!([file["id"], pic["id"]]);
    let (st, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@coder see attached", "attachments": ids})),
    )
    .await;
    assert_eq!(st, 200, "{m}");
    assert_eq!(m["message"]["attachments"].as_array().unwrap().len(), 2);
    // 同一附件不可重複綁定；別群的訊息不可綁本群附件
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&e.owner),
            Some(json!({"body": "again", "attachments": [file["id"]]}))
        )
        .await
        .0,
        400
    );
    let (_, _, b) = raw_req(&e.addr, "POST", &up("solo.txt"), &e.owner, b"solo").await;
    let solo: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{other}/messages"),
            Some(&e.owner),
            Some(json!({"body": "x", "attachments": [solo["id"]]}))
        )
        .await
        .0,
        400
    );
    let (st, m2) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"attachments": [solo["id"]]})),
    )
    .await;
    assert_eq!(st, 200, "attachment-only message: {m2}");
    // 列表帶附件
    let list = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(list[0]["attachments"][1]["kind"], "image");

    // 下載：檔案 attachment + nosniff + CSP sandbox；內容一致
    let (st, head, body) = raw_req(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/attachments/{}", file["id"]),
        &e.owner,
        b"",
    )
    .await;
    assert_eq!(st, 200);
    let h = head.to_ascii_lowercase();
    assert!(
        h.contains("content-type: application/octet-stream"),
        "{head}"
    );
    assert!(
        h.contains(
            "content-disposition: attachment; filename=download; filename*=utf-8''report%20v1.html"
        ),
        "{head}"
    );
    assert!(
        h.contains("x-content-type-options: nosniff")
            && h.contains("content-security-policy: sandbox")
    );
    assert_eq!(body, doc);
    // 圖片：inline PNG，且沒有夾帶資料
    let (st, head, body) = raw_req(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/attachments/{}", pic["id"]),
        &e.owner,
        b"",
    )
    .await;
    assert_eq!(st, 200);
    assert!(head
        .to_ascii_lowercase()
        .contains("content-type: image/png"));
    assert!(!body.windows(15).any(|w| w == b"TRAILING-SECRET"));
    assert_eq!(image::load_from_memory(&body).unwrap().width(), 40);
    // 非成員、錯的群 → 404
    assert_eq!(
        raw_req(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/attachments/{}", file["id"]),
            &e.ann,
            b""
        )
        .await
        .0,
        404
    );
    assert_eq!(
        raw_req(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{other}/attachments/{}", file["id"]),
            &e.owner,
            b""
        )
        .await
        .0,
        404
    );

    // agent 收到的提示列出附件與本機唯讀路徑（coder 的 inbox）
    let mut echo = String::new();
    for _ in 0..200 {
        let all = call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if let Some(m) = all
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["author"] == "coder")
        {
            echo = m["body"].as_str().unwrap().to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        echo.contains("read-only copy at ") && echo.contains("var/attachments/"),
        "{echo}"
    );

    // 逐字稿列出附件名與大小
    let out = tempfile::tempdir().unwrap();
    aaf_feature_chat::transcript::export(
        &e.core,
        &aaf_feature_chat::transcript::Options {
            root: out.path(),
            tz: Some("UTC"),
            rooms: Some(&[r]),
            since: None,
        },
    )
    .unwrap();
    let day = std::fs::read_dir(out.path().join(r.to_string()))
        .unwrap()
        .map(|f| f.unwrap().path())
        .find(|p| !p.ends_with("_index.md"))
        .unwrap();
    let t = std::fs::read_to_string(day).unwrap();
    assert!(
        t.contains(&format!("📎 report v1.html（{} B）", doc.len())) && t.contains("🖼 photo.png"),
        "{t}"
    );
}

#[tokio::test]
async fn attachment_size_limit_and_gc() {
    let home = tempfile::tempdir().unwrap();
    let roster = Roster {
        instance: home.path().to_path_buf(),
        ..Default::default()
    };
    let chat = Arc::new(Chat::with_tick(roster, Duration::from_millis(50)));
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Wrap(chat.clone()))).unwrap();
    let core = Core::boot(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![("AAF_CHAT_FILE_MB".into(), "1".into())],
            ..Default::default()
        },
    )
    .unwrap();
    let http: Vec<Arc<dyn HttpFeature>> = vec![chat.clone()];
    let app = aaf_api::app(core.clone(), &http);
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
    let r = call(
        &addr,
        "POST",
        "/api/v1/chat/rooms",
        Some(&t),
        Some(json!({"name": "lim"})),
    )
    .await
    .1["id"]
        .as_i64()
        .unwrap();
    let big = vec![b'x'; 1024 * 1024 + 1];
    assert_eq!(
        raw_req(
            &addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/attachments?name=big.bin"),
            &t,
            &big
        )
        .await
        .0,
        413
    );
    let (st, _, b) = raw_req(
        &addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/attachments?name=ok.bin"),
        &t,
        &big[..1000],
    )
    .await;
    assert_eq!(st, 200);
    let a: Value = serde_json::from_slice(&b).unwrap();
    let dir = home.path().join("var/attachments");
    let blobs = || {
        std::fs::read_dir(&dir)
            .unwrap()
            .filter(|f| f.as_ref().unwrap().file_name().len() == 64)
            .count()
    };
    assert_eq!(blobs(), 1);
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|f| !f
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".upload-")),
        "no temp left after 413"
    );
    // 一天前未發送的上傳 + 一小時前的內容檔 → 清除
    core.db
        .with(|c| {
            Ok(c.execute(
                "UPDATE chat_attachments SET created_at = 0 WHERE id = ?1",
                [a["id"].as_i64().unwrap()],
            )?)
        })
        .unwrap();
    let f = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200))
        .unwrap();
    assert_eq!(
        aaf_feature_chat::attachments::gc(&core, home.path()).unwrap(),
        1
    );
    assert_eq!(blobs(), 0);
}

#[tokio::test]
async fn approval_auto_allows_permission_requests() {
    let e = start("30").await;
    let r = room(&e, "auto", &["coder"], &e.owner).await;
    let coder = aaf_api::issue_role_token(&e.core, "coder", "worker").unwrap();
    let path = format!("/api/v1/chat/rooms/{r}/approval");
    assert_eq!(
        call(&e.addr, "GET", &path, Some(&e.owner), None).await.1["on"],
        false
    );
    // agent 不能開；日期檢查
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &path,
            Some(&coder),
            Some(json!({"on": true}))
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &path,
            Some(&e.owner),
            Some(json!({"on": true, "until": "2000-01-01"}))
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &path,
            Some(&e.owner),
            Some(json!({"on": true, "until": "nope"}))
        )
        .await
        .0,
        400
    );
    let (st, b) = call(
        &e.addr,
        "POST",
        &path,
        Some(&e.owner),
        Some(json!({"on": true, "until": "2999-12-31", "reason": "overnight run"})),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    assert_eq!(
        (b["approval"]["on"].as_bool(), b["changed"].as_bool()),
        (Some(true), Some(true))
    );
    // 相同設定不重複公告
    assert_eq!(
        call(
            &e.addr,
            "POST",
            &path,
            Some(&e.owner),
            Some(json!({"on": true, "until": "2999-12-31"}))
        )
        .await
        .1["changed"],
        false
    );
    // 權限請求被自動允許：假 agent 回 "PERMISSION: allow"
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@coder NEED_PERMISSION"})),
    )
    .await;
    let mut reply = String::new();
    for _ in 0..200 {
        let all = call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if let Some(m) = all
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["author"] == "coder")
        {
            reply = m["body"].as_str().unwrap().to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(reply, "PERMISSION: allow");
    let evs = e.core.bus.since(0, 5000).unwrap();
    let auto = evs
        .iter()
        .find(|x| x.kind == "runtime.permission.auto_approved")
        .expect("auto-approve recorded");
    assert_eq!(
        (
            auto.payload["option"].as_str(),
            auto.payload["ok"].as_bool()
        ),
        (Some("allow"), Some(true))
    );
    // 公告：系統訊息、不喚醒
    let all = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    let sys: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["author_kind"] == "system")
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert_eq!(sys.len(), 1);
    assert!(
        sys[0].starts_with("Auto-approve ON by alice (until 2999-12-31")
            && sys[0].contains("overnight run")
    );
    // 到期：視為關閉 → 權限請求回到人工
    e.core
        .db
        .with(|c| {
            Ok(c.execute(
                "UPDATE chat_room_approval SET until = '2001-01-01' WHERE room = ?1",
                [r],
            )?)
        })
        .unwrap();
    let g = call(&e.addr, "GET", &path, Some(&e.owner), None).await.1;
    assert_eq!(
        (g["on"].as_bool(), g["expired"].as_bool()),
        (Some(false), Some(true))
    );
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@coder NEED_PERMISSION again"})),
    )
    .await;
    let mut pending = false;
    for _ in 0..200 {
        let l = call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/lanes"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if l["lanes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| !x["pending_permissions"].as_array().unwrap().is_empty())
        {
            pending = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(pending, "expired approval leaves the request for a human");
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/lanes/coder/cancel"),
        Some(&e.owner),
        None,
    )
    .await;
}

#[tokio::test]
async fn folders_and_member_announcement() {
    let e = start("30").await;
    let a = room(&e, "a", &[], &e.owner).await;
    let b = room(&e, "b", &[], &e.owner).await;
    room(&e, "c", &[], &e.owner).await;
    for (r, f) in [(a, "work"), (b, "work")] {
        call(
            &e.addr,
            "PATCH",
            &format!("/api/v1/chat/rooms/{r}"),
            Some(&e.owner),
            Some(json!({"folder": f})),
        )
        .await;
    }
    assert_eq!(
        call(&e.addr, "GET", "/api/v1/chat/folders", Some(&e.owner), None)
            .await
            .1,
        json!([{"name": "work", "rooms": 2}])
    );
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{a}/members"),
        Some(&e.owner),
        Some(json!({"member": "coder"})),
    )
    .await;
    let m = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{a}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(m[0]["author_kind"], "system");
    assert!(m[0]["body"]
        .as_str()
        .unwrap()
        .starts_with("alice added coder"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let m = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{a}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(
        m.as_array().unwrap().len(),
        1,
        "system announcements wake nobody"
    );
}

#[tokio::test]
async fn search_messages() {
    let e = start("30").await;
    let a = room(&e, "alpha", &[], &e.owner).await;
    let b = room(&e, "beta", &[], &e.owner).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{a}/members"),
        Some(&e.owner),
        Some(json!({"member": "ann"})),
    )
    .await;
    let post = |r: i64, t: String, body: &'static str| {
        let addr = e.addr.clone();
        async move {
            call(
                &addr,
                "POST",
                &format!("/api/v1/chat/rooms/{r}/messages"),
                Some(&t),
                Some(json!({"body": body})),
            )
            .await
            .1["message"]["id"]
                .as_i64()
                .unwrap()
        }
    };
    let m1 = post(a, e.owner.clone(), "部署流程要先跑 migration 再重啟").await;
    post(a, e.ann.clone(), "Deploy failed on staging, rolling back").await;
    post(a, e.owner.clone(), "@ann can you check the deploy logs?").await;
    post(b, e.owner.clone(), "secret deploy plan for beta").await;
    let s = |tok: String, qs: &str| {
        let (addr, qs) = (e.addr.clone(), qs.to_string());
        async move {
            call(
                &addr,
                "GET",
                &format!("/api/v1/chat/search?{qs}"),
                Some(&tok),
                None,
            )
            .await
        }
    };
    let ids = |v: &Value| {
        v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    // 英文不分大小寫、新到舊；Owner 看得到所有群
    let (st, r) = s(e.owner.clone(), "q=DEPLOY").await;
    assert_eq!(st, 200);
    assert_eq!(ids(&r).len(), 3);
    assert_eq!(r["hits"][0]["room_name"], "beta");
    // 非成員看不到 beta
    let (_, r) = s(e.ann.clone(), "q=deploy").await;
    assert_eq!(ids(&r).len(), 2);
    assert!(r["hits"].as_array().unwrap().iter().all(|h| h["room"] == a));
    // 中文：3 字以上走索引、2 字走 LIKE
    assert_eq!(
        ids(
            &s(e.owner.clone(), "q=%E9%83%A8%E7%BD%B2%E6%B5%81%E7%A8%8B")
                .await
                .1
        ),
        vec![m1]
    ); // 部署流程
    assert_eq!(
        ids(&s(e.owner.clone(), "q=%E9%87%8D%E5%95%9F").await.1),
        vec![m1]
    ); // 重啟
       // 多詞 AND、片語
    assert_eq!(
        ids(&s(e.owner.clone(), "q=deploy+staging").await.1).len(),
        1
    );
    assert_eq!(
        ids(&s(e.owner.clone(), "q=%22rolling+back%22").await.1).len(),
        1
    );
    assert_eq!(
        ids(&s(e.owner.clone(), "q=%22back+rolling%22").await.1).len(),
        0
    );
    let qb = format!("q=deploy&room={b}");
    // 篩選：作者、提及我、群、只有篩選沒有關鍵字
    assert_eq!(
        ids(&s(e.owner.clone(), "q=deploy&from=ann").await.1).len(),
        1
    );
    assert_eq!(ids(&s(e.ann.clone(), "mentions=true").await.1).len(), 1);
    assert_eq!(ids(&s(e.owner.clone(), &qb).await.1).len(), 1);
    // 指定別人的群 → 404；空查詢 → 400；特殊字元不出錯
    assert_eq!(s(e.ann.clone(), &qb).await.0, 404);
    assert_eq!(s(e.owner.clone(), "q=").await.0, 400);
    for weird in [
        "q=%22",
        "q=a%2A",
        "q=NEAR(",
        "q=%25",
        "q=_",
        "q=%27%3B+DROP+TABLE",
    ] {
        assert_eq!(s(e.owner.clone(), weird).await.0, 200, "{weird}");
    }
    // 摘要帶命中位置
    let (_, r) = s(e.owner.clone(), "q=staging").await;
    assert!(r["hits"][0]["snippet"]
        .as_str()
        .unwrap()
        .contains("staging"));
    // 垃圾桶的群不搜
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{b}/state"),
        Some(&e.owner),
        Some(json!({"action": "delete"})),
    )
    .await;
    assert_eq!(ids(&s(e.owner.clone(), "q=secret").await.1).len(), 0);
    // 跳到訊息：around 回傳以該則為中心的訊息
    let around = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{a}/messages?around={m1}"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert!(around.as_array().unwrap().iter().any(|m| m["id"] == m1));
}

#[tokio::test]
async fn latest_and_around_windows() {
    let e = start("30").await;
    let r = room(&e, "long", &[], &e.owner).await;
    let mut ids = vec![];
    for i in 0..30 {
        ids.push(
            call(
                &e.addr,
                "POST",
                &format!("/api/v1/chat/rooms/{r}/messages"),
                Some(&e.owner),
                Some(json!({"body": format!("m{i}")})),
            )
            .await
            .1["message"]["id"]
                .as_i64()
                .unwrap(),
        );
    }
    let get = |qs: String| {
        let addr = e.addr.clone();
        let t = e.owner.clone();
        async move {
            call(
                &addr,
                "GET",
                &format!("/api/v1/chat/rooms/{r}/messages?{qs}"),
                Some(&t),
                None,
            )
            .await
            .1
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_i64().unwrap())
            .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        get("latest=true&limit=10".into()).await,
        ids[20..].to_vec(),
        "newest 10, oldest first"
    );
    assert_eq!(get("after=0&limit=10".into()).await, ids[..10].to_vec());
    let around = get(format!("around={}", ids[3])).await;
    assert_eq!(around.first(), Some(&ids[0]));
    assert!(around.contains(&ids[3]));
}

fn rule(dir: &std::path::Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// hooks：擋下、放行、fail-open、提醒、agent 回覆也受規則管。
#[tokio::test]
async fn hooks_block_allow_fail_open_and_notes() {
    let home = tempfile::tempdir().unwrap();
    let hooks = home.path().join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let tmp = std::env::temp_dir();
    let mut roster = Roster {
        instance: home.path().to_path_buf(),
        ..Default::default()
    };
    // echo 角色：回覆＝收到的提示（用來觀察 wake 提醒是否附上）
    roster.roles.insert(
        "echo".into(),
        RoleSpec::new(
            DriverSpec::Command {
                command: vec!["/bin/cat".into()],
                cwd: None,
            },
            &tmp,
        ),
    );
    let chat = Arc::new(Chat::with_tick(roster, Duration::from_millis(50)));
    chat.set_hooks(
        aaf_runtime::hooks::Hooks::new(&hooks, home.path()).with_timeout(Duration::from_secs(2)),
    );
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Wrap(chat.clone()))).unwrap();
    let core = Core::boot(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    let http: Vec<Arc<dyn HttpFeature>> = vec![chat.clone()];
    let app = aaf_api::app(core.clone(), &http);
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
    let r = call(
        &addr,
        "POST",
        "/api/v1/chat/rooms",
        Some(&t),
        Some(json!({"name": "rules"})),
    )
    .await
    .1["id"]
        .as_i64()
        .unwrap();
    let post = |body: &str| {
        let (addr, t, body) = (addr.clone(), t.clone(), body.to_string());
        async move {
            call(
                &addr,
                "POST",
                &format!("/api/v1/chat/rooms/{r}/messages"),
                Some(&t),
                Some(json!({"body": body})),
            )
            .await
        }
    };

    // 沒有規則：一切照常
    assert_eq!(post("hello").await.0, 200);
    // 規則：pre 擋含 FORBIDDEN 的發文（讀 stdin JSON）
    rule(&hooks, "10_block.sh", "#!/bin/sh\n[ \"$AAF_HOOK_PHASE\" = pre ] || exit 0\ngrep -q FORBIDDEN && echo 'no forbidden words please'\nexit 0\n");
    let (st, b) = post("this is FORBIDDEN").await;
    assert_eq!(st, 422, "{b}");
    assert!(b["message"]
        .as_str()
        .unwrap()
        .contains("no forbidden words please"));
    assert_eq!(post("this is fine").await.0, 200);
    // fail-open：崩潰、非 0、逾時、非執行檔、symlink 都放行並記錄
    rule(&hooks, "20_crash.sh", "#!/bin/sh\necho oops >&2\nexit 3\n");
    rule(&hooks, "30_slow.sh", "#!/bin/sh\nsleep 10\necho late\n");
    std::fs::write(
        hooks.join("40_noexec.sh"),
        "#!/bin/sh\necho 'should not run'\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(hooks.join("10_block.sh"), hooks.join("50_link.sh")).unwrap();
    let t0 = std::time::Instant::now();
    assert_eq!(post("still fine").await.0, 200);
    assert!(
        t0.elapsed() < Duration::from_secs(6),
        "timeout enforced: {:?}",
        t0.elapsed()
    );
    let log = std::fs::read_to_string(home.path().join("var/hooks.log")).unwrap();
    assert!(
        log.contains("20_crash.sh: exited")
            && log.contains("oops")
            && log.contains("30_slow.sh: timed out"),
        "{log}"
    );
    assert!(log.contains("blocked chat.post alice by 10_block.sh"));
    std::fs::remove_file(hooks.join("30_slow.sh")).unwrap();
    // 規則看不到 token
    rule(&hooks, "60_env.sh", "#!/bin/sh\n[ \"$AAF_HOOK_PHASE\" = pre ] || exit 0\nenv | grep -q TOKEN && echo 'token leaked'\nexit 0\n");
    assert_eq!(post("env check").await.0, 200);

    // agent 回覆也受規則管：echo 回覆的內容含 FORBIDDEN → 被擋、不發；下次叫醒時告訴它原因
    call(
        &addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/members"),
        Some(&t),
        Some(json!({"member": "echo"})),
    )
    .await;
    assert_eq!(
        post("@echo say FORBIDDEN").await.0,
        422,
        "a person's post is checked first"
    );
    // 讓 echo 收到不含關鍵字的提示，但規則改成擋 agent 的回覆（rank=worker）
    rule(&hooks, "10_block.sh", "#!/bin/sh\n[ \"$AAF_HOOK_PHASE\" = pre ] || exit 0\ngrep -q '\"rank\":\"worker\"' && echo 'agents must not echo'\nexit 0\n");
    rule(
        &hooks,
        "70_post.sh",
        "#!/bin/sh\n[ \"$AAF_HOOK_PHASE\" = post ] && echo 'remember the status line'\nexit 0\n",
    );
    assert_eq!(post("@echo first").await.0, 200);
    let mut blocked = false;
    for _ in 0..200 {
        if core
            .bus
            .since(0, 5000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "chat.message.blocked")
        {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(blocked, "agent reply blocked");
    let msgs = call(
        &addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&t),
        None,
    )
    .await
    .1;
    assert!(
        msgs.as_array()
            .unwrap()
            .iter()
            .all(|m| m["author"] != "echo"),
        "nothing posted by echo"
    );
    // 放行 agent，再叫一次：提示最前面附上「回覆被擋」的提醒
    std::fs::remove_file(hooks.join("10_block.sh")).unwrap();
    assert_eq!(post("@echo second").await.0, 200);
    let mut got = String::new();
    for _ in 0..200 {
        let all = call(
            &addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/messages"),
            Some(&t),
            None,
        )
        .await
        .1;
        if let Some(m) = all
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["author"] == "echo")
        {
            got = m["body"].as_str().unwrap().to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        got.starts_with("[AA Forum rules]\n- Your reply in room"),
        "{got}"
    );
    assert!(got.contains("blocked by rule: agents must not echo"));
    // echo 第二次回覆通過後，post 規則的提醒存給 echo（下次叫醒才附上）
    let mut noted = false;
    for _ in 0..80 {
        if std::fs::read_to_string(home.path().join("var/hook_notes/echo.jsonl"))
            .is_ok_and(|s| s.contains("remember the status line"))
        {
            noted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(noted, "post note stored for the agent");
}

#[tokio::test]
async fn hooks_task_post_and_wake_turn_end() {
    let home = tempfile::tempdir().unwrap();
    let hooks_dir = home.path().join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    rule(&hooks_dir, "a.sh", "#!/bin/sh\ncase \"$AAF_HOOK_PHASE\" in\n pre) grep -q '\"type\":\"task.post\"' && echo 'no tasks today';;\n wake) echo 'wake rule';;\n turn_end) echo 'after turn';;\nesac\nexit 0\n");
    let h = aaf_runtime::hooks::Hooks::new(&hooks_dir, home.path());
    assert_eq!(
        h.pre(&json!({"type": "task.post", "sender": "alice"}))
            .await
            .as_deref(),
        Some("no tasks today")
    );
    assert_eq!(
        h.pre(&json!({"type": "chat.post", "sender": "alice"}))
            .await,
        None
    );
    h.turn_end(&json!({"type": "turn_end", "role": "coder"}))
        .await;
    h.turn_end(&json!({"type": "turn_end", "role": "coder"}))
        .await;
    let w = h.wake(&json!({"type": "wake", "role": "coder"})).await;
    assert_eq!(
        w, "[AA Forum rules]\n- after turn\n- wake rule\n\n",
        "notes deduped, then wake output"
    );
    assert_eq!(
        h.wake(&json!({"type": "wake", "role": "coder"})).await,
        "[AA Forum rules]\n- wake rule\n\n",
        "notes cleared after one use"
    );
    // 名稱不合法的角色不寫檔（防路徑穿越）
    h.add_notes("../x", &["n".into()]);
    assert!(!home.path().join("var/x.jsonl").exists());
}

#[tokio::test]
async fn consecutive_invites_merge_into_one_system_notice() {
    let e = start("30").await;
    let r = room(&e, "merge", &[], &e.owner).await;
    for who in ["coder", "reviewer", "echo"] {
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/rooms/{r}/members"),
            Some(&e.owner),
            Some(json!({"member": who})),
        )
        .await;
    }
    let m = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    let sys: Vec<&Value> = m
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["author_kind"] == "system")
        .collect();
    assert_eq!(sys.len(), 1, "{m}");
    assert_eq!(
        sys[0]["body"],
        "alice added coder, reviewer, echo to the room."
    );
}

// ---------------- 停滯清單 ----------------

async fn new_task(e: &Env, r: i64, title: &str, assignee: Option<&str>) -> i64 {
    let (st, t) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks"),
        Some(&e.owner),
        Some(json!({"title": title, "assignee": assignee})),
    )
    .await;
    assert_eq!(st, 200, "{t}");
    t["id"].as_i64().unwrap()
}

/// 等本群連續 0.3 秒沒有進行中的回合。
async fn settle_room(e: &Env, r: i64) {
    let d = e.chat.shared().dispatcher(&e.core);
    let mut quiet = 0;
    for _ in 0..400 {
        if d.busy_in_room(r) {
            quiet = 0;
        } else {
            quiet += 1;
            if quiet >= 6 {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("room {r} still has turns running after 20 s");
}

fn age(e: &Env, id: i64, secs: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    e.core
        .db
        .with(|c| {
            Ok(c.execute(
                "UPDATE chat_tasks SET updated_at = ?1 WHERE id = ?2",
                rusqlite::params![now - secs, id],
            )?)
        })
        .unwrap();
}

async fn stalled(e: &Env, r: i64, token: &str) -> (u16, Value) {
    call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/stalled"),
        Some(token),
        None,
    )
    .await
}

fn ids(v: &Value) -> Vec<i64> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn stalled_threshold_states_and_lease_expiry() {
    let e = start_with("30", |r| {
        r.roles.get_mut("reviewer").unwrap().rank = "lead".into()
    })
    .await;
    let r = room(&e, "stall", &["coder", "reviewer"], &e.owner).await;
    let before = new_task(&e, r, "before", Some("coder")).await;
    let exact = new_task(&e, r, "exact", Some("coder")).await;
    let after = new_task(&e, r, "after", None).await;
    let done = new_task(&e, r, "done", Some("coder")).await;
    let cancelled = new_task(&e, r, "cancelled", None).await;
    let blocked = new_task(&e, r, "blocked", Some("coder")).await;
    let coder = role_token(&e.core, "coder", "worker");
    for (id, st) in [(done, "done"), (blocked, "blocked")] {
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/tasks/{id}/claim"),
            Some(&coder),
            None,
        )
        .await;
        call(
            &e.addr,
            "POST",
            &format!("/api/v1/chat/tasks/{id}/report"),
            Some(&coder),
            Some(json!({"state": st, "result": "x"})),
        )
        .await;
    }
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{cancelled}/report"),
        Some(&e.owner),
        Some(json!({"state": "cancelled", "result": ""})),
    )
    .await;
    // 停滯清單會略過「負責人正在跑回合」的任務：先等派工通知引起的回合都跑完（慢的 CI 機器上會還在跑）
    settle_room(&e, r).await;
    age(&e, before, 30 * 60 - 5);
    age(&e, exact, 30 * 60);
    for id in [after, done, cancelled, blocked] {
        age(&e, id, 31 * 60);
    }
    let (st, v) = stalled(&e, r, &e.owner).await;
    assert_eq!(st, 200, "{v}");
    let mut got = ids(&v);
    got.sort();
    assert_eq!(got, vec![exact, after, blocked]);
    let by = |id: i64| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by(blocked)["kind"], "waiting_external");
    assert_eq!(by(exact)["kind"], "stalled");
    assert_eq!(by(exact)["wake_target"], "coder");
    // 沒有負責人 → 本群帶隊
    assert_eq!(by(after)["wake_target"], "reviewer");
    assert!(by(after)["idle_secs"].as_i64().unwrap() >= 31 * 60);

    // 租約到期回 open，但 updated_at 不重設：仍在清單
    let leased = new_task(&e, r, "leased", Some("coder")).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{leased}/claim"),
        Some(&coder),
        None,
    )
    .await;
    age(&e, leased, 40 * 60);
    e.core
        .db
        .with(|c| {
            Ok(c.execute(
                "UPDATE chat_tasks SET lease_until = 1 WHERE id = ?1",
                [leased],
            )?)
        })
        .unwrap();
    let v = stalled(&e, r, &e.owner).await.1;
    let l = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == leased)
        .expect("leased listed")
        .clone();
    assert_eq!(l["state"], "open");
    assert!(l["idle_secs"].as_i64().unwrap() >= 40 * 60);
    // 任務本身有更新（認領）就離開清單
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/tasks/{leased}/claim"),
        Some(&coder),
        None,
    )
    .await;
    assert!(!ids(&stalled(&e, r, &e.owner).await.1).contains(&leased));
}

#[tokio::test]
async fn wake_posts_once_and_guards() {
    let e = start_with("30", |r| {
        r.roles.get_mut("reviewer").unwrap().rank = "lead".into()
    })
    .await;
    let r = room(&e, "wake", &["coder", "reviewer"], &e.owner).await;
    let id = new_task(&e, r, "stuck job", Some("coder")).await;
    let fresh = new_task(&e, r, "fresh", Some("coder")).await;
    age(&e, id, 45 * 60);
    let wake = |tid: i64, tok: String| {
        let a = e.addr.clone();
        async move {
            call(
                &a,
                "POST",
                &format!("/api/v1/chat/rooms/{r}/tasks/{tid}/wake"),
                Some(&tok),
                None,
            )
            .await
        }
    };
    // 非成員 403
    assert_eq!(wake(id, e.ann.clone()).await.0, 403);
    assert_eq!(stalled(&e, r, &e.ann).await.0, 403);
    // 不在清單 409
    assert_eq!(wake(fresh, e.owner.clone()).await.0, 409);
    let (st, v) = wake(id, e.owner.clone()).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["wake_target"], "coder");
    let (st2, v2) = wake(id, e.owner.clone()).await;
    assert_eq!(st2, 409);
    assert!(v2.to_string().contains("已於"), "{v2}");
    // 系統訊息只一則，且只投遞給 coder
    let n: Vec<(i64, String)> = e
        .core
        .db
        .with(|c| {
            let mut st = c.prepare("SELECT id, body FROM chat_messages WHERE room = ?1 AND author_kind = 'system' AND body LIKE '%沒有進度%'")?;
            let v = st.query_map([r], |x| Ok((x.get(0)?, x.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
        .unwrap();
    assert_eq!(n.len(), 1, "{n:?}");
    assert!(
        n[0].1
            .starts_with(&format!("@coder 任務 #{id}「stuck job」已 45 分鐘沒有進度")),
        "{}",
        n[0].1
    );
    let to: Vec<String> = e
        .core
        .db
        .with(|c| {
            let mut st = c.prepare("SELECT recipient FROM chat_deliveries WHERE message = ?1")?;
            let v = st
                .query_map([n[0].0], |x| x.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
        .unwrap();
    assert_eq!(to, vec!["coder".to_string()]);
    assert!(e
        .core
        .bus
        .since(0, 5000)
        .unwrap()
        .iter()
        .any(|ev| ev.kind == "chat.task.woken"));
}

#[tokio::test]
async fn no_lead_means_no_wake_target() {
    let e = start("30").await;
    let r = room(&e, "nolead", &["coder"], &e.owner).await;
    let id = new_task(&e, r, "orphan", None).await;
    age(&e, id, 60 * 60);
    let v = stalled(&e, r, &e.owner).await.1;
    assert!(v[0]["wake_target"].is_null());
    assert!(v[0]["wake_note"].as_str().unwrap().contains("沒有帶隊"));
    let (st, b) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks/{id}/wake"),
        Some(&e.owner),
        None,
    )
    .await;
    assert_eq!(st, 409, "{b}");
}

#[tokio::test]
async fn busy_target_not_listed() {
    let e = start("30").await;
    let r = room(&e, "busy", &["coder"], &e.owner).await;
    let id = new_task(&e, r, "running", Some("coder")).await;
    age(&e, id, 60 * 60);
    assert_eq!(ids(&stalled(&e, r, &e.owner).await.1), vec![id]);
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@coder SLOW"})),
    )
    .await;
    let mut seen_busy = false;
    for _ in 0..200 {
        let l = call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{r}/lanes"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if l["lanes"][0]["status"] == "working" {
            seen_busy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen_busy);
    assert!(ids(&stalled(&e, r, &e.owner).await.1).is_empty());
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/lanes/coder/cancel"),
        Some(&e.owner),
        None,
    )
    .await;
}

#[tokio::test]
async fn wake_title_mentions_do_not_wake_others() {
    let e = start("30").await;
    let r = room(&e, "title", &["coder", "reviewer"], &e.owner).await;
    let id = new_task(&e, r, "ask @reviewer and @echo", Some("coder")).await;
    age(&e, id, 45 * 60);
    let notices = |m: &[Value]| {
        m.iter()
            .filter(|m| {
                m["author_kind"] == "system" && m["body"].as_str().unwrap().contains("不在本群")
            })
            .count()
    };
    let n0 = notices(&wait_messages(&e, r, 1).await);
    let (st, v) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks/{id}/wake"),
        Some(&e.owner),
        None,
    )
    .await;
    assert_eq!(st, 200, "{v}");
    let to: Vec<String> = e
        .core
        .db
        .with(|c| {
            let mut st = c.prepare("SELECT d.recipient FROM chat_deliveries d JOIN chat_task_wakes w ON w.message = d.message WHERE w.task = ?1")?;
            let v = st.query_map([id], |x| x.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
        .unwrap();
    assert_eq!(to, vec!["coder".to_string()]);
    assert_eq!(
        notices(&wait_messages(&e, r, 1).await),
        n0,
        "wake adds no undelivered notice"
    );
}

#[tokio::test]
async fn stall_minutes_config_and_zero_disables() {
    let e = start_env("30", &[("AAF_STALL_MINUTES", "5")], |_| {}).await;
    let r = room(&e, "cfg", &["coder"], &e.owner).await;
    let a = new_task(&e, r, "ten", Some("coder")).await;
    let b = new_task(&e, r, "four", Some("coder")).await;
    age(&e, a, 10 * 60);
    age(&e, b, 4 * 60);
    assert_eq!(ids(&stalled(&e, r, &e.owner).await.1), vec![a]);

    let e = start_env("30", &[("AAF_STALL_MINUTES", "0")], |_| {}).await;
    let r = room(&e, "off", &["coder"], &e.owner).await;
    let a = new_task(&e, r, "old", Some("coder")).await;
    age(&e, a, 24 * 3600);
    assert!(ids(&stalled(&e, r, &e.owner).await.1).is_empty());
    let (st, b) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks/{a}/wake"),
        Some(&e.owner),
        None,
    )
    .await;
    assert_eq!(st, 409);
    assert!(b.to_string().contains("停滯清單已關閉"), "{b}");
}

async fn audit_start_config(
    retention_days: &str,
    tweak: impl FnOnce(&mut Roster),
    stall: Option<&str>,
) -> Env {
    match stall {
        Some(v) => start_env(retention_days, &[("AAF_STALL_MINUTES", v)], tweak).await,
        None => start_with(retention_days, tweak).await,
    }
}

// Edge cases for waking a stalled task from the stall list.
async fn audit_wake(e: &Env, r: i64, id: i64, token: &str) -> (u16, Value) {
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks/{id}/wake"),
        Some(token),
        None,
    )
    .await
}
fn audit_wakes(e: &Env, id: i64) -> i64 {
    e.core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM chat_task_wakes WHERE task=?1",
                [id],
                |row| row.get(0),
            )?)
        })
        .unwrap()
}
fn audit_wake_recipients(e: &Env, id: i64) -> Vec<String> {
    e.core.db.with(|c| {
        let mut q=c.prepare("SELECT d.recipient FROM chat_deliveries d JOIN chat_task_wakes w ON w.message=d.message WHERE w.task=?1 ORDER BY d.recipient")?;
        let rows=q.query_map([id], |row| row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }).unwrap()
}

#[tokio::test]
async fn audit_title_cannot_add_wake_recipients() {
    let e = start("30").await;
    let r = room(&e, "title", &["coder", "reviewer"], &e.owner).await;
    let id = new_task(&e, r, "追蹤 @reviewer 的問題", Some("coder")).await;
    age(&e, id, 3600);
    let (status, v) = audit_wake(&e, r, id, &e.owner).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["wake_target"], "coder");
    let recipients = audit_wake_recipients(&e, id);
    println!(
        "computed_target={} actual_recipients={recipients:?}",
        v["wake_target"]
    );
    assert_eq!(
        recipients,
        vec!["coder"],
        "title introduced an extra wake recipient"
    );
}

#[tokio::test]
async fn audit_zero_setting_disables_stall_list() {
    let e = audit_start_config("30", |_| {}, Some("0")).await;
    let r = room(&e, "off", &["coder"], &e.owner).await;
    let id = new_task(&e, r, "old", Some("coder")).await;
    age(&e, id, 3600);
    let (status, v) = stalled(&e, r, &e.owner).await;
    assert_eq!(status, 200, "{v}");
    println!("AAF_STALL_MINUTES=0 response={v}");
    assert!(ids(&v).is_empty(), "disabled setting still lists work");
    assert_eq!(audit_wake(&e, r, id, &e.owner).await.0, 409);
    assert_eq!(audit_wakes(&e, id), 0);
}

#[tokio::test]
async fn audit_custom_threshold_is_respected() {
    let e = audit_start_config("30", |_| {}, Some("5")).await;
    let r = room(&e, "custom", &["coder"], &e.owner).await;
    let id = new_task(&e, r, "ten minutes", Some("coder")).await;
    age(&e, id, 600);
    let (status, v) = stalled(&e, r, &e.owner).await;
    assert_eq!(status, 200, "{v}");
    println!("AAF_STALL_MINUTES=5 response={v}");
    assert_eq!(ids(&v), vec![id], "custom threshold ignored");
}

#[tokio::test]
async fn audit_parallel_wake_is_single_delivery() {
    let e = start("30").await;
    let r = room(&e, "race", &["coder"], &e.owner).await;
    let id = new_task(&e, r, "same task", Some("coder")).await;
    age(&e, id, 3600);
    let (a, b) = tokio::join!(
        audit_wake(&e, r, id, &e.owner),
        audit_wake(&e, r, id, &e.owner)
    );
    let mut statuses = vec![a.0, b.0];
    statuses.sort();
    println!("parallel_statuses={statuses:?}");
    assert_eq!(statuses, vec![200, 409]);
    assert_eq!(audit_wakes(&e, id), 1);
    assert_eq!(audit_wake_recipients(&e, id), vec!["coder"]);
}

#[tokio::test]
async fn audit_completed_since_listing_cannot_wake() {
    let e = start("30").await;
    let r = room(&e, "completed", &["coder"], &e.owner).await;
    let id = new_task(&e, r, "finish", Some("coder")).await;
    age(&e, id, 3600);
    assert_eq!(ids(&stalled(&e, r, &e.owner).await.1), vec![id]);
    e.core
        .db
        .with(|c| {
            c.execute("UPDATE chat_tasks SET state='done' WHERE id=?1", [id])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(audit_wake(&e, r, id, &e.owner).await.0, 409);
    assert_eq!(audit_wakes(&e, id), 0);
}

#[tokio::test]
async fn audit_removed_assignee_falls_back_inside_room() {
    let e = start_with("30", |roster| {
        roster.roles.get_mut("reviewer").unwrap().rank = "lead".into();
    })
    .await;
    let r = room(&e, "removed", &["coder", "reviewer"], &e.owner).await;
    let id = new_task(&e, r, "handoff", Some("coder")).await;
    age(&e, id, 3600);
    assert_eq!(stalled(&e, r, &e.owner).await.1[0]["wake_target"], "coder");
    e.core
        .db
        .with(|c| {
            c.execute(
                "DELETE FROM chat_members WHERE room=?1 AND member='coder'",
                [r],
            )?;
            Ok(())
        })
        .unwrap();
    let (status, v) = audit_wake(&e, r, id, &e.owner).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["wake_target"], "reviewer");
    assert_eq!(audit_wake_recipients(&e, id), vec!["reviewer"]);
}

#[tokio::test]
async fn audit_cross_room_task_id_has_no_side_effect() {
    let e = start("30").await;
    let r1 = room(&e, "first", &["coder"], &e.owner).await;
    let r2 = room(&e, "second", &["coder"], &e.owner).await;
    let id = new_task(&e, r2, "belongs to second", Some("coder")).await;
    age(&e, id, 3600);
    assert_eq!(audit_wake(&e, r1, id, &e.owner).await.0, 404);
    assert_eq!(audit_wakes(&e, id), 0);
    assert_eq!(audit_wake(&e, r2, id, &e.ann).await.0, 403);
    assert_eq!(audit_wakes(&e, id), 0);
}

// ---------------- 群級 lead ----------------

async fn set_lead(e: &Env, r: i64, role: Option<&str>, token: &str) -> (u16, Value) {
    call(
        &e.addr,
        "PUT",
        &format!("/api/v1/chat/rooms/{r}/lead"),
        Some(token),
        Some(json!({"role": role})),
    )
    .await
}

async fn post_task(e: &Env, r: i64, token: &str) -> u16 {
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{r}/tasks"),
        Some(token),
        Some(json!({"title": "t"})),
    )
    .await
    .0
}

#[tokio::test]
async fn room_lead_grants_task_post_only_in_that_room() {
    let e = start_with("30", |r| {
        r.roles.get_mut("reviewer").unwrap().rank = "lead".into()
    })
    .await;
    let a = room(&e, "A", &["coder", "reviewer"], &e.owner).await;
    let b = room(&e, "B", &["coder", "reviewer"], &e.owner).await;
    let coder = role_token(&e.core, "coder", "worker");
    let reviewer = role_token(&e.core, "reviewer", "lead");
    // 沒指定：沿用全域 rank
    assert_eq!(post_task(&e, a, &coder).await, 403);
    assert_eq!(post_task(&e, a, &reviewer).await, 200);
    // A 群指定 coder
    let (st, v) = set_lead(&e, a, Some("coder"), &e.owner).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["lead"], "coder");
    assert_eq!(post_task(&e, a, &coder).await, 200);
    // 群有指定以群為準：全域 lead 在 A 群失去派工權；B 群照舊
    assert_eq!(post_task(&e, a, &reviewer).await, 403);
    assert_eq!(post_task(&e, b, &coder).await, 403);
    assert_eq!(post_task(&e, b, &reviewer).await, 200);
    // 不改全域 rank
    assert_eq!(
        call(&e.addr, "GET", "/api/v1/chat/whoami", Some(&coder), None)
            .await
            .1["rank"],
        "worker"
    );
    // 群資訊與系統訊息
    let info = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{a}"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert_eq!(info["lead"], "coder");
    let msgs = wait_messages(&e, a, 1).await;
    assert!(msgs
        .iter()
        .any(|m| m["author_kind"] == "system" && m["body"] == "alice 指定 coder 為本群 lead。"));
    // 撤換立即失效，退回全域
    assert_eq!(set_lead(&e, a, None, &e.owner).await.0, 200);
    assert_eq!(post_task(&e, a, &coder).await, 403);
    assert_eq!(post_task(&e, a, &reviewer).await, 200);
    let msgs = wait_messages(&e, a, 1).await;
    assert!(msgs.iter().any(|m| m["author_kind"] == "system"
        && m["body"]
            .as_str()
            .unwrap()
            .starts_with("alice 撤換本群 lead（原為 coder）")));
}

#[tokio::test]
async fn room_lead_rejects_bad_requests_without_side_effects() {
    let e = start("30").await;
    let r = room(&e, "guard", &["coder"], &e.owner).await;
    let lead_tok = role_token(&e.core, "coder", "lead");
    // 非 owner（人、角色）
    assert_eq!(set_lead(&e, r, Some("coder"), &e.ann).await.0, 403);
    assert_eq!(set_lead(&e, r, Some("coder"), &lead_tok).await.0, 403);
    // 群外角色、不存在角色
    assert_eq!(set_lead(&e, r, Some("reviewer"), &e.owner).await.0, 409);
    assert_eq!(set_lead(&e, r, Some("ghost"), &e.owner).await.0, 409);
    let info = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert!(info["lead"].is_null());
    let msgs = call(
        &e.addr,
        "GET",
        &format!("/api/v1/chat/rooms/{r}/messages"),
        Some(&e.owner),
        None,
    )
    .await
    .1;
    assert!(!msgs
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["body"].as_str().unwrap().contains("lead")));
}

#[tokio::test]
async fn room_lead_removed_from_room_loses_power_and_wake_falls_back() {
    let e = start_with("30", |r| {
        r.roles.get_mut("reviewer").unwrap().rank = "lead".into()
    })
    .await;
    let a = room(&e, "A", &["coder", "reviewer"], &e.owner).await;
    let b = room(&e, "B", &["coder", "reviewer"], &e.owner).await;
    let coder = role_token(&e.core, "coder", "worker");
    set_lead(&e, a, Some("coder"), &e.owner).await;
    // 停滯任務沒負責人：A 群叫醒群級 lead，B 群叫醒全域 lead
    let ta = new_task(&e, a, "orphan a", None).await;
    let tb = new_task(&e, b, "orphan b", None).await;
    age(&e, ta, 3600);
    age(&e, tb, 3600);
    assert_eq!(stalled(&e, a, &e.owner).await.1[0]["wake_target"], "coder");
    assert_eq!(
        stalled(&e, b, &e.owner).await.1[0]["wake_target"],
        "reviewer"
    );
    // 舊 token 在被移出群後不再放行，也不會改叫全域 lead
    e.core
        .db
        .with(|c| {
            Ok(c.execute(
                "DELETE FROM chat_members WHERE room = ?1 AND member = 'coder'",
                [a],
            )?)
        })
        .unwrap();
    // 已不是成員：看不到這個群（404），派工被拒
    assert_eq!(post_task(&e, a, &coder).await, 404);
    let s = stalled(&e, a, &e.owner).await.1;
    assert!(s[0]["wake_target"].is_null(), "{s}");
    assert!(s[0]["wake_note"].as_str().unwrap().contains("coder"));
    // Shared::room_lead 只回該群的指定（給 skill 包用）
    let sh = e.chat.shared();
    let _ = sh.dispatcher(&e.core);
    assert_eq!(sh.room_lead(a).as_deref(), Some("coder"));
    assert_eq!(sh.room_lead(b), None);
}

#[tokio::test]
async fn whoami_reports_rank_in_room() {
    let e = start_with("30", |r| {
        r.roles.get_mut("reviewer").unwrap().rank = "lead".into()
    })
    .await;
    let a = room(&e, "A", &["coder", "reviewer"], &e.owner).await;
    let b = room(&e, "B", &["coder", "reviewer"], &e.owner).await;
    let coder = role_token(&e.core, "coder", "worker");
    let reviewer = role_token(&e.core, "reviewer", "lead");
    set_lead(&e, a, Some("coder"), &e.owner).await;
    let who = |tok: String, q: &'static str| {
        let addr = e.addr.clone();
        async move {
            call(
                &addr,
                "GET",
                &format!("/api/v1/chat/whoami{q}"),
                Some(&tok),
                None,
            )
            .await
            .1
        }
    };
    let qa: &'static str = Box::leak(format!("?room={a}").into_boxed_str());
    let qb: &'static str = Box::leak(format!("?room={b}").into_boxed_str());
    // 被指定者：A 群 lead、B 群 worker；不帶 room 維持全域
    assert_eq!(
        who(coder.clone(), qa).await,
        json!({"name": "coder", "rank": "lead", "room": a, "global_rank": "worker"})
    );
    assert_eq!(who(coder.clone(), qb).await["rank"], "worker");
    assert_eq!(
        who(coder.clone(), "").await,
        json!({"name": "coder", "rank": "worker"})
    );
    // 全域 lead 在已指定他人的群＝worker；沒指定的群＝lead
    assert_eq!(who(reviewer.clone(), qa).await["rank"], "worker");
    assert_eq!(who(reviewer.clone(), qb).await["rank"], "lead");
    // 人帶 room 不變
    assert_eq!(
        who(e.owner.clone(), qa).await,
        json!({"name": "alice", "rank": "owner"})
    );
}

// ── 回合失敗不能無聲消失（真 HTTP＋SQLite＋假 ACP）────────────────

async fn wait_system(e: &Env, room: i64, needle: &str) -> Value {
    for _ in 0..400 {
        let m = call(
            &e.addr,
            "GET",
            &format!("/api/v1/chat/rooms/{room}/messages"),
            Some(&e.owner),
            None,
        )
        .await
        .1;
        if let Some(x) = m.as_array().unwrap().iter().find(|m| {
            m["author_kind"] == "system" && m["body"].as_str().unwrap_or("").contains(needle)
        }) {
            return x.clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no system notice containing {needle:?} in room {room}");
}

fn delivery_state(e: &Env, msg: i64, who: &str) -> String {
    e.core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT state FROM chat_deliveries WHERE message=?1 AND recipient=?2",
                rusqlite::params![msg, who],
                |r| r.get(0),
            )?)
        })
        .unwrap()
}

#[tokio::test]
async fn turn_failure_notice_mentions_room_lead_and_acks() {
    let e = start_env("30", &[("AAF_TURN_RETRY", "0")], |r| {
        r.roles.get_mut("reviewer").unwrap().rank = "lead".into()
    })
    .await;
    let rid = room(&e, "fail", &["coder", "reviewer"], &e.owner).await;
    let (st, m) = call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{rid}/messages"),
        Some(&e.owner),
        Some(json!({"body": "@coder ALWAYS_FAIL please"})),
    )
    .await;
    assert_eq!(st, 200, "{m}");
    let id = m["message"]["id"].as_i64().unwrap();
    let n = wait_system(&e, rid, "沒有完成").await;
    let body = n["body"].as_str().unwrap();
    assert!(body.starts_with("@reviewer "), "{body}");
    assert!(
        body.contains(&format!("#{id}")) && body.contains("coder"),
        "{body}"
    );
    for _ in 0..100 {
        if delivery_state(&e, id, "coder") == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(delivery_state(&e, id, "coder"), "done");
    // 公告只叫醒帶隊（不叫醒失敗的角色自己）
    let rcpt: Vec<String> = e
        .core
        .db
        .with(|c| {
            let mut st = c.prepare("SELECT recipient FROM chat_deliveries WHERE message = ?1")?;
            let v = st
                .query_map([n["id"].as_i64().unwrap()], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(v)
        })
        .unwrap();
    assert_eq!(rcpt, vec!["reviewer".to_string()]);
}

#[tokio::test]
async fn turn_failure_notice_without_lead_mentions_nobody() {
    let e = start_env("30", &[("AAF_TURN_RETRY", "0")], |_| {}).await;
    let rid = room(&e, "fail2", &["coder"], &e.owner).await;
    call(
        &e.addr,
        "POST",
        &format!("/api/v1/chat/rooms/{rid}/messages"),
        Some(&e.owner),
        Some(json!({"body": "STOP_REFUSAL"})),
    )
    .await;
    let n = wait_system(&e, rid, "模型拒絕回答").await;
    assert!(n["body"].as_str().unwrap().starts_with("⚠ 系統："));
    let cnt: i64 = e
        .core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM chat_deliveries WHERE message=?1",
                [n["id"].as_i64().unwrap()],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(cnt, 0);
}

#[tokio::test]
async fn turn_retry_config_default_and_override() {
    let e = start_env("30", &[], |_| {}).await;
    assert_eq!(e.chat.shared().dispatcher(&e.core).turn_retry, 2);
    let e = start_env("30", &[("AAF_TURN_RETRY", "5")], |_| {}).await;
    assert_eq!(e.chat.shared().dispatcher(&e.core).turn_retry, 5);
    let e = start_env("30", &[("AAF_TURN_RETRY", "0")], |_| {}).await;
    assert_eq!(e.chat.shared().dispatcher(&e.core).turn_retry, 0);
}

/// AAF_LEAD_SEES_ALL：群裡每則非系統訊息都送給本群 lead（lead 自己的發言不送給自己）。
#[tokio::test]
async fn lead_receives_every_room_message() {
    let e = start_with("30", |r| {
        r.roles.get_mut("reviewer").unwrap().rank = "lead".into()
    })
    .await;
    let r = room(&e, "lead-sees-all", &["coder", "reviewer"], &e.owner).await;
    let coder = role_token(&e.core, "coder", "worker");
    let reviewer = role_token(&e.core, "reviewer", "lead");
    let post = |tok: String, body: &'static str| {
        let addr = e.addr.clone();
        async move {
            let (st, m) = call(
                &addr,
                "POST",
                &format!("/api/v1/chat/rooms/{r}/messages"),
                Some(&tok),
                Some(json!({ "body": body })),
            )
            .await;
            assert_eq!(st, 200, "{m}");
            let mut to: Vec<String> = serde_json::from_value(m["delivered_to"].clone()).unwrap();
            to.sort();
            to
        }
    };
    // 角色沒 @ 人：以前誰都不叫醒，現在送給 lead
    assert_eq!(
        post(coder.clone(), "status: working").await,
        vec!["reviewer"]
    );
    // 角色 @ lead：只送一次
    assert_eq!(
        post(coder.clone(), "@reviewer done").await,
        vec!["reviewer"]
    );
    // 人 @ 別人：lead 也收到
    assert_eq!(
        post(e.owner.clone(), "@coder do x").await,
        vec!["coder", "reviewer"]
    );
    // lead 自己沒 @ 的發言：不送給自己、也不叫醒別人
    assert!(post(reviewer.clone(), "noted").await.is_empty());
}
