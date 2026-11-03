//! 按停止中斷回合：被中斷的訊息不可無聲消失（AAF_STOP_NOTICE）。
//! 真 ChatInbox + SQLite：取消／停止後強制關閉都要在群裡留系統公告列出沒處理完的 id，
//! @ 寄信的其他角色（不 @ 被停止的角色、不 @ 人）；公告寫不進去時批次留在佇列。
use aaf_core::feature::BootOptions;
use aaf_core::storage::Db;
use aaf_core::{Core, Registry};
use aaf_feature_chat::{inbox::ChatInbox, Chat};
use aaf_runtime::dispatcher::{shared_roster, Dispatcher, Lane, RoleSpec, Roster};
use aaf_runtime::{AgentStatus, DriverSpec};
use std::sync::Arc;
use std::time::Duration;

struct Env {
    db: Db,
    core: Arc<Core>,
    d: Dispatcher,
    lane: Lane,
}

/// 群 1：tester（人）寄 #1、helper（角色）寄 #2 給 coder；第一封內容決定 fake agent 行為（SLOW／HANG）。
fn env(first: &str, stop_notice: bool) -> Env {
    env_with(self_fake_acp(), first, stop_notice)
}

fn env_with(coder: DriverSpec, first: &str, stop_notice: bool) -> Env {
    let out = tempfile::tempdir().unwrap().keep();
    let mut r = Roster {
        instance: out.clone(),
        ..Default::default()
    };
    r.roles.insert("coder".into(), RoleSpec::new(coder, &out));
    r.roles
        .insert("helper".into(), RoleSpec::new(DriverSpec::Manual, &out));
    let mut reg = Registry::default();
    reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
    reg.register(Box::new(Chat::new(r.clone()))).unwrap();
    let db = Db::open(&out.join("evidence.sqlite")).unwrap();
    let core = Core::boot(&reg, db.clone(), BootOptions::default()).unwrap();
    db.with(|c| {
        c.execute(
            "INSERT INTO chat_rooms(id,name,created_by,created_at) VALUES(1,'f','tester',1)",
            [],
        )?;
        c.execute_batch(
            "INSERT INTO chat_members(room,member,kind,added_at) VALUES(1,'coder','role',1),(1,'helper','role',1);",
        )?;
        c.execute(
            "INSERT INTO chat_messages(id,room,author,author_kind,body,created_at) VALUES(1,1,'tester','user',?1,1),(2,1,'helper','role','@coder 資料在這',2)",
            [first],
        )?;
        c.execute_batch(
            "INSERT INTO chat_deliveries(message,recipient,state) VALUES(1,'coder','queued'),(2,'coder','queued');",
        )?;
        Ok(())
    })
    .unwrap();
    let inbox = Arc::new(ChatInbox {
        db: db.clone(),
        bus: core.bus.clone(),
        roster: shared_roster(r.clone()),
        hooks: None,
    });
    let mut d = Dispatcher::new(inbox, core.bus.clone(), r).with_stop_notice(stop_notice);
    d.stop_grace = Duration::from_millis(150);
    d.stall_kill = Duration::ZERO;
    Env {
        db,
        core,
        d,
        lane: Lane::new("coder", Some(1)),
    }
}

async fn start_and_stop(e: &Env) {
    e.d.tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !e
            .d
            .lane_states()
            .await
            .iter()
            .any(|(l, s)| l == &e.lane && *s == AgentStatus::Working)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(e.d.cancel(&e.lane).await.unwrap());
}

async fn settle(e: &Env) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while e.d.busy(&e.lane) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn states(db: &Db) -> Vec<(i64, String)> {
    db.with(|c| {
        let mut s = c.prepare(
            "SELECT message,state FROM chat_deliveries WHERE recipient='coder' ORDER BY message",
        )?;
        let v = s
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .unwrap()
}

fn system_posts(db: &Db) -> Vec<String> {
    db.with(|c| {
        let mut s = c.prepare("SELECT body FROM chat_messages WHERE author_kind='system'")?;
        let v = s
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .unwrap()
}

fn assert_notice(db: &Db) {
    let n: Vec<String> = system_posts(db)
        .into_iter()
        .filter(|b| b.contains("手動停止"))
        .collect();
    assert_eq!(n.len(), 1, "exactly one stop notice: {n:?}");
    let b = &n[0];
    assert!(b.contains("#1") && b.contains("#2"), "{b}");
    assert!(b.contains("@helper"), "role author is asked to resend: {b}");
    assert!(!b.contains("@coder"), "stopped role must not be woken: {b}");
    assert!(!b.contains("@tester"), "humans are not @-ed: {b}");
    assert_eq!(
        states(db),
        vec![(1, "done".to_string()), (2, "done".to_string())]
    );
}

/// 合作取消（agent 回 stopReason=cancelled）
#[tokio::test]
async fn cancel_posts_notice_listing_unfinished_ids() {
    let e = env("SLOW work", true);
    start_and_stop(&e).await;
    settle(&e).await;
    assert_notice(&e.db);
    // 之後的信照常處理（不自動重跑被停止的那批）
    e.db.with(|c| {
        c.execute_batch("INSERT INTO chat_messages(id,room,author,author_kind,body,created_at) VALUES(100,1,'tester','user','after',3); INSERT INTO chat_deliveries(message,recipient,state) VALUES(100,'coder','queued');")?;
        Ok(())
    })
    .unwrap();
    e.d.once().await.unwrap();
    let echo: i64 =
        e.db.with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM chat_messages WHERE author='coder' AND body='ECHO: after'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(echo, 1);
    e.d.close(&e.lane, false).await.unwrap();
}

/// agent 不理會取消，寬限期後強制關閉（cancel_ignored）
#[tokio::test]
async fn stop_then_force_close_posts_notice() {
    let e = env("HANG", true);
    start_and_stop(&e).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(e.d.reap_stuck().await.unwrap(), vec![e.lane.clone()]);
    settle(&e).await;
    assert_notice(&e.db);
    assert!(
        !system_posts(&e.db).iter().any(|b| b.contains("強制中斷")),
        "user stop is not reported as the watchdog"
    );
}

/// 公告寫不進去（回合中群被封存）→ 批次留在佇列、記 notice_failed
#[tokio::test]
async fn stop_notice_rejected_keeps_batch_queued() {
    let e = env("SLOW work", true);
    let mut ev = e.core.bus.subscribe();
    e.d.tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !e
            .d
            .lane_states()
            .await
            .iter()
            .any(|(l, s)| l == &e.lane && *s == AgentStatus::Working)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    e.db.with(|c| {
        c.execute("UPDATE chat_rooms SET state='archived' WHERE id=1", [])?;
        Ok(())
    })
    .unwrap();
    assert!(e.d.cancel(&e.lane).await.unwrap());
    settle(&e).await;
    assert_eq!(
        states(&e.db),
        vec![(1, "queued".to_string()), (2, "queued".to_string())]
    );
    assert!(system_posts(&e.db).is_empty());
    let mut failed = false;
    while let Ok(Ok(x)) = tokio::time::timeout(Duration::from_millis(500), ev.next()).await {
        if x.kind == "runtime.lane.notice_failed" {
            failed = true;
            break;
        }
    }
    assert!(failed);
    e.d.close(&e.lane, false).await.unwrap();
}

/// AAF_STOP_NOTICE=0：回到舊行為（安靜 ack）
#[tokio::test]
async fn stop_notice_off_is_quiet() {
    let e = env("SLOW work", false);
    start_and_stop(&e).await;
    settle(&e).await;
    assert!(system_posts(&e.db).is_empty());
    assert_eq!(
        states(&e.db),
        vec![(1, "done".to_string()), (2, "done".to_string())]
    );
    e.d.close(&e.lane, false).await.unwrap();
}

/// 服務關機（aaf up 收到 SIGTERM／Ctrl-C）中斷回合：不公告、不 ack，批次留在佇列等重啟；
/// agent 已停、lane 已關，之後不再派工。ACP（中斷時回 Err）與 command（回 Ok(cancelled)）都一樣。
#[tokio::test]
async fn shutdown_mid_turn_keeps_batch_queued_and_stops_dispatch() {
    shutdown_mid_turn(env("HANG", true)).await;
}

#[tokio::test]
async fn shutdown_mid_turn_command_driver_is_quiet_too() {
    let sleep = DriverSpec::Command {
        command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        cwd: None,
    };
    shutdown_mid_turn(env_with(sleep, "work", true)).await;
}

async fn shutdown_mid_turn(e: Env) {
    e.d.tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !e
            .d
            .lane_states()
            .await
            .iter()
            .any(|(l, s)| l == &e.lane && *s == AgentStatus::Working)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(e.d.shutdown().await.is_empty(), "agent stopped");
    // shutdown 等回合 task 收尾才回傳
    assert!(!e.d.busy(&e.lane), "turn finished before shutdown returned");
    assert!(e.d.lane_states().await.is_empty(), "lane closed");
    assert!(system_posts(&e.db).is_empty());
    assert_eq!(
        states(&e.db),
        vec![(1, "queued".to_string()), (2, "queued".to_string())]
    );
    assert!(
        e.d.tick().await.unwrap().is_empty(),
        "no dispatch after shutdown"
    );
    assert!(!e.d.busy(&e.lane));
}

fn self_fake_acp() -> DriverSpec {
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
