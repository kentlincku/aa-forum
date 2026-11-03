//! 回合失敗公告契約：被中斷或失敗的訊息一定要在群裡留下可見的公告。為了在 CI 自足：
//! 1. AUDIT_SQLITE_FIXTURE 未設時用暫存目錄；2. AUDIT_FAKE_ACP 未設時用本測試檔自身扮演假 ACP；
//! 3. 公告為平台系統訊息（author_kind=system），故「可見公告」查詢納入 system。
use aaf_core::feature::BootOptions;
use aaf_core::storage::Db;
use aaf_core::{Core, Registry};
use aaf_feature_chat::{inbox::ChatInbox, Chat};
use aaf_runtime::dispatcher::{shared_roster, Dispatcher, Lane, RoleSpec, Roster};
use aaf_runtime::{AgentStatus, DriverSpec};
use std::sync::Arc;
use std::time::Duration;

async fn scenario(blocked: bool) {
    let out = std::env::var("AUDIT_SQLITE_FIXTURE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| tempfile::tempdir().unwrap().keep())
        .join(if blocked { "blocked" } else { "normal" });
    std::fs::create_dir_all(&out).unwrap();
    let path = out.join("evidence.sqlite");
    assert!(!path.exists(), "use a fresh owned fixture directory");
    let mut r = Roster {
        instance: out.clone(),
        ..Default::default()
    };
    let spec = match std::env::var("AUDIT_FAKE_ACP") {
        Ok(c) => DriverSpec::Acp {
            command: vec![c],
            acp_agent: None,
            model: None,
            mode: None,
            cwd: None,
            env: vec![],
            effort: None,
            account: None,
            transport: None,
        },
        Err(_) => self_fake_acp(),
    };
    r.roles.insert("coder".into(), RoleSpec::new(spec, &out));
    let mut reg = Registry::default();
    reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
    reg.register(Box::new(Chat::new(r.clone()))).unwrap();
    let db = Db::open(&path).unwrap();
    let core = Core::boot(&reg, db.clone(), BootOptions::default()).unwrap();
    db.with(|c| {
        c.execute_batch("INSERT INTO chat_rooms(id,name,created_by,created_at) VALUES(1,'auditor fixture','tester',1);
        INSERT INTO chat_members(room,member,kind,added_at) VALUES(1,'coder','role',1);
        INSERT INTO chat_messages(id,room,author,author_kind,body,created_at) VALUES(1,1,'tester','user','HANG original unfinished',1),(2,1,'tester','user','original batch companion',2);
        INSERT INTO chat_deliveries(message,recipient,state) VALUES(1,'coder','queued'),(2,'coder','queued');")?;
        Ok(())
    }).unwrap();
    let hooks = if blocked {
        use std::os::unix::fs::PermissionsExt;
        let dir = out.join("hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("guard.py");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json,os,sys
v=json.load(sys.stdin)
if os.environ.get('AAF_HOOK_PHASE')=='pre' and '⚠ 系統：' in v.get('body',''):
    print('fixture: runtime notice rejected by room rule')
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(aaf_runtime::hooks::Hooks::new(dir, out.clone()))
    } else {
        None
    };
    let inbox = Arc::new(ChatInbox {
        db: db.clone(),
        bus: core.bus.clone(),
        roster: shared_roster(r.clone()),
        hooks,
    });
    let mut d = Dispatcher::new(inbox, core.bus.clone(), r);
    d.stall_kill = Duration::from_millis(150);
    let lane = Lane::new("coder", Some(1));
    d.tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !d
            .lane_states()
            .await
            .iter()
            .any(|(l, s)| l == &lane && *s == AgentStatus::Working)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    db.with(|c| { c.execute_batch("INSERT INTO chat_messages(id,room,author,author_kind,body,created_at) VALUES(3,1,'tester','user','new queued instruction',3); INSERT INTO chat_deliveries(message,recipient,state) VALUES(3,'coder','queued');")?; Ok(()) }).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(d.reap_stuck().await.unwrap(), vec![lane.clone()]);
    tokio::time::timeout(Duration::from_secs(5), async {
        while d.busy(&lane) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let states = db
        .with(|c| {
            let mut s = c.prepare("SELECT message,state FROM chat_deliveries ORDER BY message")?;
            let v = s
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .unwrap();
    let reply_count: i64 = db
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM chat_messages WHERE author_kind='role'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    println!(
        "SQLITE after watchdog before relaunch states={states:?} role_reply_count={reply_count}"
    );

    let notices = db
        .with(|c| {
            let mut s =
                c.prepare("SELECT body FROM chat_messages WHERE author_kind IN ('role','system')")?;
            let v = s
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .unwrap();
    let visible = notices
        .iter()
        .any(|t| t.contains("強制中斷") && t.contains("#1") && t.contains("#2"));
    println!(
        "AUDIT blocked={blocked} notice_visible={visible} states={states:?} notices={notices:?}"
    );
    if blocked {
        let originals_done = states
            .iter()
            .any(|(id, state)| (*id == 1 || *id == 2) && state == "done");
        let q_pending = states
            .iter()
            .any(|(id, state)| *id == 3 && state == "queued");
        d.close(&lane, false).await.unwrap();
        assert!(q_pending);
        assert!(visible || !originals_done, "watchdog silently acked unfinished work when the interruption notice was not actually posted");
        return;
    }
    assert_eq!(
        states,
        vec![(1, "done".into()), (2, "done".into()), (3, "queued".into())]
    );
    d.once().await.unwrap();
    d.close(&lane, false).await.unwrap();
    let replies = db
        .with(|c| {
            let mut s = c.prepare("SELECT body FROM chat_messages WHERE author_kind='role'")?;
            let v = s
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .unwrap();
    assert!(replies.iter().any(|x| x == "ECHO: new queued instruction"));
    println!("AUDIT blocked={blocked} queued_later_replied=true");
    assert!(visible, "watchdog silently acked unfinished work when the interruption notice was not actually posted");
    println!(
        "SQLITE after relaunch replies={replies:?} db={}",
        path.display()
    );
}

#[tokio::test]
async fn audit_visible_notice_real_chat_inbox_normal() {
    scenario(false).await;
}
#[tokio::test]
async fn audit_visible_notice_real_chat_inbox_rule_blocked() {
    scenario(true).await;
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

/// 公告寫不進去（群已封存）：原批次不可標成已處理，並記 runtime.lane.notice_failed。
#[tokio::test]
async fn watchdog_notice_failure_keeps_batch_queued() {
    let out = tempfile::tempdir().unwrap().keep();
    let path = out.join("evidence.sqlite");
    let mut r = Roster {
        instance: out.clone(),
        ..Default::default()
    };
    r.roles
        .insert("coder".into(), RoleSpec::new(self_fake_acp(), &out));
    let mut reg = Registry::default();
    reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
    reg.register(Box::new(Chat::new(r.clone()))).unwrap();
    let db = Db::open(&path).unwrap();
    let core = Core::boot(&reg, db.clone(), BootOptions::default()).unwrap();
    db.with(|c| {
        c.execute_batch(
            "INSERT INTO chat_rooms(id,name,created_by,created_at) VALUES(1,'f','tester',1);
             INSERT INTO chat_members(room,member,kind,added_at) VALUES(1,'coder','role',1);
             INSERT INTO chat_messages(id,room,author,author_kind,body,created_at) VALUES(1,1,'tester','user','HANG',1);
             INSERT INTO chat_deliveries(message,recipient,state) VALUES(1,'coder','queued');",
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
    let mut d = Dispatcher::new(inbox, core.bus.clone(), r);
    d.stall_kill = Duration::from_millis(150);
    let lane = Lane::new("coder", Some(1));
    let mut ev = core.bus.subscribe();
    d.tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !d
            .lane_states()
            .await
            .iter()
            .any(|(l, s)| l == &lane && *s == AgentStatus::Working)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    db.with(|c| {
        c.execute("UPDATE chat_rooms SET state='archived' WHERE id=1", [])?;
        Ok(())
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(d.reap_stuck().await.unwrap(), vec![lane.clone()]);
    tokio::time::timeout(Duration::from_secs(5), async {
        while d.busy(&lane) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let state: String = db
        .with(|c| {
            Ok(c.query_row(
                "SELECT state FROM chat_deliveries WHERE message=1",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        state, "queued",
        "notice not written ⇒ batch must stay queued"
    );
    let mut failed = false;
    while let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(500), ev.next()).await {
        if e.kind == "runtime.lane.notice_failed" {
            failed = true;
            break;
        }
    }
    assert!(failed);
    d.close(&lane, false).await.unwrap();
}

/// 回合失敗公告寫不進去（回合中群被封存） → 原批次留在佇列、記 notice_failed，不可無聲消失。
#[tokio::test]
async fn turn_failure_notice_rejected_keeps_batch_queued() {
    let out = tempfile::tempdir().unwrap().keep();
    let mut r = Roster {
        instance: out.clone(),
        ..Default::default()
    };
    let spec = aaf_runtime::DriverSpec::Command {
        command: vec!["/bin/sh".into(), "-c".into(), "sleep 0.4; exit 3".into()],
        cwd: None,
    };
    r.roles.insert("coder".into(), RoleSpec::new(spec, &out));
    let mut reg = Registry::default();
    reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
    reg.register(Box::new(Chat::new(r.clone()))).unwrap();
    let db = Db::open(&out.join("evidence.sqlite")).unwrap();
    let core = Core::boot(&reg, db.clone(), BootOptions::default()).unwrap();
    db.with(|c| {
        c.execute_batch(
            "INSERT INTO chat_rooms(id,name,created_by,created_at) VALUES(1,'f','tester',1);
             INSERT INTO chat_members(room,member,kind,added_at) VALUES(1,'coder','role',1);
             INSERT INTO chat_messages(id,room,author,author_kind,body,created_at) VALUES(1,1,'tester','user','ALWAYS_FAIL',1);
             INSERT INTO chat_deliveries(message,recipient,state) VALUES(1,'coder','queued');",
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
    let mut d = Dispatcher::new(inbox, core.bus.clone(), r).with_turn_retry(0);
    d.retry_backoff = vec![Duration::ZERO];
    let lane = Lane::new("coder", Some(1));
    let mut ev = core.bus.subscribe();
    d.tick().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    db.with(|c| {
        c.execute("UPDATE chat_rooms SET state='archived' WHERE id=1", [])?;
        Ok(())
    })
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while d.busy(&lane) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let state: String = db
        .with(|c| {
            Ok(c.query_row(
                "SELECT state FROM chat_deliveries WHERE message=1",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        state, "queued",
        "notice not written ⇒ batch must stay queued"
    );
    let sys: i64 = db
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM chat_messages WHERE author_kind='system'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(sys, 0);
    let mut failed = false;
    while let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(500), ev.next()).await {
        if e.kind == "runtime.lane.notice_failed" {
            failed = true;
            break;
        }
    }
    assert!(failed);
    d.close(&lane, false).await.unwrap();
}
