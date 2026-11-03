//! 回合進行中的新信（AAF_STEER／AAF_BUSY_NOTICE_MINUTES），真 ChatInbox + SQLite。
//! - Hermes lane：新信用 `/steer` 即時插入進行中的回合，插入成功即標成已處理，不另開回合；
//!   Hermes 的回條不可混進角色回覆。
//! - 插不進去（非 Hermes 或關閉 steer）：忙超過門檻就公告一次「忙碌中、已排隊」，信留在佇列，下輪照常處理。
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
    _core: Arc<Core>,
    d: Dispatcher,
    lane: Lane,
}

fn env(first: &str, hermes: bool, tweak: impl FnOnce(Dispatcher) -> Dispatcher) -> Env {
    let out = tempfile::tempdir().unwrap().keep();
    let mut r = Roster {
        instance: out.clone(),
        ..Default::default()
    };
    r.roles
        .insert("coder".into(), RoleSpec::new(self_fake_acp(hermes), &out));
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
        c.execute(
            "INSERT INTO chat_members(room,member,kind,added_at) VALUES(1,'coder','role',1)",
            [],
        )?;
        c.execute(
            "INSERT INTO chat_messages(id,room,author,author_kind,body,created_at) VALUES(1,1,'tester','user',?1,1)",
            [first],
        )?;
        c.execute(
            "INSERT INTO chat_deliveries(message,recipient,state) VALUES(1,'coder','queued')",
            [],
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
    let d = tweak(Dispatcher::new(inbox, core.bus.clone(), r));
    Env {
        db,
        _core: core,
        d,
        lane: Lane::new("coder", Some(1)),
    }
}

fn post(db: &Db, id: i64, body: &str) {
    db.with(|c| {
        c.execute(
            "INSERT INTO chat_messages(id,room,author,author_kind,body,created_at) VALUES(?1,1,'tester','user',?2,?1)",
            rusqlite::params![id, body],
        )?;
        c.execute(
            "INSERT INTO chat_deliveries(message,recipient,state) VALUES(?1,'coder','queued')",
            [id],
        )?;
        Ok(())
    })
    .unwrap();
}

fn state(db: &Db, id: i64) -> String {
    db.with(|c| {
        Ok(c.query_row(
            "SELECT state FROM chat_deliveries WHERE message=?1 AND recipient='coder'",
            [id],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

fn bodies(db: &Db, kind: &str) -> Vec<String> {
    db.with(|c| {
        let mut s = c.prepare("SELECT body FROM chat_messages WHERE author_kind=?1 ORDER BY id")?;
        let v = s
            .query_map([kind], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .unwrap()
}

async fn working(e: &Env) {
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
}

/// tick 直到條件成立（插入是背景任務）
async fn until(e: &Env, f: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            e.d.tick().await.unwrap();
            if f() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
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

#[tokio::test]
async fn hermes_lane_gets_new_messages_mid_turn() {
    let e = env("STEERABLE long job", true, |d| d);
    working(&e).await;
    post(&e.db, 100, "note A");
    until(&e, || state(&e.db, 100) == "done").await;
    assert!(e.d.busy(&e.lane), "still the same turn");
    post(&e.db, 101, "FINISH now");
    until(&e, || state(&e.db, 101) == "done").await;
    settle(&e).await;
    assert_eq!(state(&e.db, 1), "done");
    let replies = bodies(&e.db, "role");
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0], "STEERED: note A | FINISH now");
    assert!(
        !replies[0].contains("Steer queued"),
        "Hermes steer acknowledgement must not leak into the role reply"
    );
    let started: Vec<String> = e
        .db
        .with(|c| {
            let mut s = c.prepare(
                "SELECT kind||' '||payload FROM events WHERE kind LIKE 'runtime.turn.%' ORDER BY seq",
            )?;
            let v = s
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .unwrap();
    assert!(
        started
            .iter()
            .filter(|x| x.starts_with("runtime.turn.started"))
            .all(|x| x.contains("\"messages\":[1]")),
        "steered messages do not start new turns: {started:#?}"
    );
    assert_eq!(
        started
            .iter()
            .filter(|x| x.starts_with("runtime.turn.steered"))
            .count(),
        2
    );
    assert!(bodies(&e.db, "system").is_empty());
    e.d.close(&e.lane, false).await.unwrap();
}

/// 沒有進行中的回合時 driver 不送 `/steer`（交給一般派工）
#[tokio::test]
async fn steer_without_active_turn_is_not_running() {
    use aaf_runtime::Driver;
    let out = tempfile::tempdir().unwrap();
    let d = aaf_runtime::acp::AcpDriver::new(
        match self_fake_acp(true) {
            DriverSpec::Acp { command, .. } => command,
            _ => unreachable!(),
        },
        out.path().to_path_buf(),
        vec![("AAF_FAKE_ACP".into(), "1".into())],
    );
    d.start().await.unwrap();
    assert_eq!(
        d.steer("x").await.unwrap(),
        aaf_runtime::SteerOutcome::NotRunning
    );
    d.stop().await.unwrap();
}

#[tokio::test]
async fn busy_notice_when_steer_unavailable() {
    let e = env("SLOW job", false, |mut d| {
        d.busy_notice_after = Duration::from_millis(1);
        d
    });
    working(&e).await;
    post(&e.db, 100, "are you there");
    until(&e, || !bodies(&e.db, "system").is_empty()).await;
    // 再 tick 幾次也只公告一次
    for _ in 0..5 {
        e.d.tick().await.unwrap();
    }
    let sys = bodies(&e.db, "system");
    assert_eq!(sys.len(), 1, "{sys:?}");
    assert!(sys[0].contains("#100") && sys[0].contains("正在忙") && !sys[0].contains("#1、"));
    assert_eq!(
        state(&e.db, 100),
        "queued",
        "not steered into a non-Hermes lane"
    );
    e.d.cancel(&e.lane).await.unwrap();
    settle(&e).await;
    // 下一輪照常處理
    e.d.once().await.unwrap();
    assert_eq!(state(&e.db, 100), "done");
    assert!(bodies(&e.db, "role")
        .iter()
        .any(|b| b == "ECHO: are you there"));
    e.d.close(&e.lane, false).await.unwrap();
}

#[tokio::test]
async fn steer_off_keeps_old_queueing() {
    let e = env("STEERABLE long job", true, |d| {
        d.with_steer(false).with_busy_notice(Duration::ZERO)
    });
    working(&e).await;
    post(&e.db, 100, "note A");
    for _ in 0..10 {
        e.d.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state(&e.db, 100), "queued");
    assert!(bodies(&e.db, "system").is_empty());
    e.d.cancel(&e.lane).await.unwrap();
    settle(&e).await;
    e.d.close(&e.lane, false).await.unwrap();
}

fn self_fake_acp(hermes: bool) -> DriverSpec {
    DriverSpec::Acp {
        command: vec![
            std::env::current_exe().unwrap().to_string_lossy().into(),
            "--exact".into(),
            "fake_agent_entry".into(),
            "--nocapture".into(),
        ],
        acp_agent: hermes.then(|| "hermes".into()),
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
