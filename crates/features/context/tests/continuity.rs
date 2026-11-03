use aaf_core::{feature::BootOptions, storage::Db, Core, Registry};
use aaf_feature_context::Context;
use aaf_runtime::dispatcher::{Dispatcher, Inbox, InboxMessage, Lane, RoleSpec, Roster};
use aaf_runtime::DriverSpec;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Mail {
    queued: Mutex<Vec<InboxMessage>>,
    replies: Mutex<Vec<String>>,
    sessions: Mutex<std::collections::HashMap<Lane, String>>,
}
impl Mail {
    fn post(&self, id: i64, body: &str) {
        self.queued.lock().unwrap().push(InboxMessage {
            id,
            from: "owner".into(),
            body: body.into(),
            room: Some(7),
            thread_id: None,
        });
    }
}
#[async_trait]
impl Inbox for Mail {
    async fn lanes_with_unread(&self) -> aaf_core::Result<Vec<Lane>> {
        Ok(self
            .queued
            .lock()
            .unwrap()
            .iter()
            .map(|m| Lane::new("writer", m.room))
            .collect())
    }
    async fn unread(&self, lane: &Lane) -> aaf_core::Result<Vec<InboxMessage>> {
        Ok(self
            .queued
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.room == lane.room)
            .cloned()
            .collect())
    }
    async fn ack(&self, _: &str, ids: &[i64]) -> aaf_core::Result<()> {
        self.queued.lock().unwrap().retain(|m| !ids.contains(&m.id));
        Ok(())
    }
    async fn reply(&self, _: &str, _: &InboxMessage, text: &str) -> aaf_core::Result<()> {
        self.replies.lock().unwrap().push(text.into());
        Ok(())
    }
    async fn load_session(&self, l: &Lane) -> aaf_core::Result<Option<String>> {
        Ok(self.sessions.lock().unwrap().get(l).cloned())
    }
    async fn save_session(&self, l: &Lane, s: &str) -> aaf_core::Result<()> {
        self.sessions.lock().unwrap().insert(l.clone(), s.into());
        Ok(())
    }
    async fn forget_sessions(&self, _: &str) -> aaf_core::Result<()> {
        self.sessions.lock().unwrap().clear();
        Ok(())
    }
}
fn core(db: Db) -> Arc<Core> {
    let mut r = Registry::default();
    r.register(Box::new(Context)).unwrap();
    Arc::new(
        Core::prepare(
            &r,
            db,
            BootOptions {
                env: vec![],
                ..Default::default()
            },
        )
        .unwrap(),
    )
}
fn roster(home: &std::path::Path) -> Roster {
    let mut r = Roster {
        instance: home.into(),
        ..Default::default()
    };
    r.roles.insert(
        "writer".into(),
        RoleSpec::new(
            DriverSpec::Command {
                command: vec!["/bin/cat".into()],
                cwd: None,
            },
            home,
        ),
    );
    r
}
fn dispatcher(core: &Arc<Core>, mail: Arc<Mail>, roster: Roster) -> Dispatcher {
    Dispatcher::new(mail, core.bus.clone(), roster).with_continuity(Some(Arc::new(
        aaf_feature_context::Ledger::configured(core).unwrap(),
    )))
}

#[tokio::test]
async fn switched_lane_recovers_canary_and_next_action() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open(&home.path().join("state.db")).unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), roster(home.path()));
    mail.post(
        1,
        "Decision CANARY-ORCHID. Next action: verify dirty artifact before publishing.",
    );
    d.once().await.unwrap();
    mail.post(2, "ordinary follow-up");
    d.once().await.unwrap();
    assert!(!mail.replies.lock().unwrap()[1].contains("AAF CONTINUATION"));
    let mut changed = roster(home.path());
    changed.roles.get_mut("writer").unwrap().driver = DriverSpec::Command {
        command: vec!["/bin/sh".into(), "-c".into(), "cat".into()],
        cwd: None,
    };
    d.apply_roster(changed).await.unwrap();
    mail.post(3, "continue previous work");
    d.once().await.unwrap();
    let reply = mail.replies.lock().unwrap()[2].clone();
    assert!(
        reply.contains("CANARY-ORCHID"),
        "switch must recover durable requirements: {reply}"
    );
    assert!(reply.contains("verify dirty artifact before publishing"));
    assert!(reply.contains("AAF CONTINUATION"));
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
}
#[test]
fn fake_agent() {
    if std::env::var_os("FAKE_CONTEXT_TEST").is_some() {
        if let Ok(ms) = std::env::var("FAKE_START_DELAY_MS") {
            std::thread::sleep(std::time::Duration::from_millis(ms.parse().unwrap()));
        }
        aaf_runtime::testing::fake_acp_main();
    }
}

fn acp_roster(home: &std::path::Path, fail_load: bool) -> Roster {
    let mut r = roster(home);
    let mut env = vec![
        ("FAKE_CONTEXT_TEST".into(), "1".into()),
        ("FAKE_ECHO_PROMPT".into(), "1".into()),
    ];
    if fail_load {
        env.push(("FAKE_FAIL_LOAD".into(), "1".into()));
    }
    r.roles.get_mut("writer").unwrap().driver = DriverSpec::Acp {
        command: vec![
            std::env::current_exe().unwrap().display().to_string(),
            "--exact".into(),
            "fake_agent".into(),
            "--nocapture".into(),
        ],
        acp_agent: None,
        model: None,
        mode: None,
        cwd: None,
        env,
        effort: None,
        account: None,
        transport: None,
    };
    r
}

#[tokio::test]
async fn actual_native_load_does_not_inject_but_load_fallback_does() {
    for fail_load in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let c = core(Db::open(&home.path().join("state.db")).unwrap());
        let mail = Arc::new(Mail::default());
        let r = acp_roster(home.path(), fail_load);
        let d = dispatcher(&c, mail.clone(), r.clone());
        mail.post(1, "OLD-CANARY next action: inspect artifact");
        d.once().await.unwrap();
        d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
        drop(d);
        let d = dispatcher(&c, mail.clone(), r);
        mail.post(2, "continue");
        d.once().await.unwrap();
        let reply = mail.replies.lock().unwrap()[1].clone();
        assert_eq!(
            reply.contains("AAF CONTINUATION"),
            fail_load,
            "actual load outcome, not DB sid: {reply}"
        );
        if fail_load {
            assert!(reply.contains("OLD-CANARY"));
        }
        mail.post(3, "ordinary third prompt");
        d.once().await.unwrap();
        assert!(!mail.replies.lock().unwrap()[2].contains("AAF CONTINUATION"));
        d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
    }
}
#[tokio::test]
async fn failed_checkpoint_does_not_ack_or_lose_recovery_on_native_load_retry() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open(&home.path().join("state.db")).unwrap());
    let mail = Arc::new(Mail::default());
    let r = acp_roster(home.path(), false);
    let d = dispatcher(&c, mail.clone(), r.clone());
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "RETRY-CANARY next action: read back dirty file");
    d.once().await.unwrap();
    d.close(&lane, true).await.unwrap();
    c.db.with(|c| {c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints BEGIN SELECT RAISE(ABORT,'injected checkpoint failure'); END;")?;Ok(())}).unwrap();
    mail.post(2, "continue");
    assert!(d.once().await.is_err());
    assert_eq!(mail.queued.lock().unwrap().len(), 1);
    assert_eq!(mail.replies.lock().unwrap().len(), 1);
    let before: i64 =
        c.db.with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM context_sources WHERE kind='output'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(
        d.once().await.is_err(),
        "storage-faulted executor must stay blocked until explicit recovery"
    );
    let after: i64 =
        c.db.with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM context_sources WHERE kind='output'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        before, after,
        "do not re-execute an unknown-effects turn on every scheduler tick"
    );
    d.close(&lane, false).await.unwrap();
    drop(d);
    c.db.with(|c| {
        c.execute_batch("DROP TRIGGER fail_checkpoint;")?;
        Ok(())
    })
    .unwrap();
    let d = dispatcher(&c, mail.clone(), r);
    d.once().await.unwrap();
    let reply = mail.replies.lock().unwrap()[1].clone();
    assert!(
        reply.contains("RETRY-CANARY"),
        "uncommitted injection must be retried even after native load: {reply}"
    );
    assert!(mail.queued.lock().unwrap().is_empty());
    c.db.with(|c| {
        let (cursor,version,pending): (i64,i64,bool) = c.query_row("SELECT injected_cursor,injected_version,pending FROM context_sessions WHERE role='writer' AND room=7",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert!(cursor > 0 && version > 0); assert!(!pending);
        Ok(())
    }).unwrap();
    d.close(&lane, false).await.unwrap();
}
#[tokio::test]
async fn switch_while_starting_cannot_close_a_selected_executor() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let mut r = acp_roster(home.path(), false);
    if let DriverSpec::Acp { env, .. } = &mut r.roles.get_mut("writer").unwrap().driver {
        env.push(("FAKE_START_DELAY_MS".into(), "300".into()));
    }
    let d = Arc::new(dispatcher(&c, mail.clone(), r));
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "RACE-CANARY next: inspect result");
    let launch = {
        let d = d.clone();
        tokio::spawn(async move { d.tick().await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while d.lane_driver(&lane).await.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    d.apply_roster(roster(home.path())).await.unwrap();
    assert!(
        launch.await.unwrap().is_ok(),
        "old launch must complete safely before roster replacement"
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while d.busy(&lane) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    mail.post(2, "queued after switch");
    d.once().await.unwrap();
    let reply = mail.replies.lock().unwrap().last().unwrap().clone();
    assert!(reply.contains("RACE-CANARY"));
    d.close(&lane, false).await.unwrap();
}
#[tokio::test]
async fn tool_evidence_is_durable_but_raw_commands_are_not_injected() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), acp_roster(home.path(), false));
    mail.post(1, "TOOL_EVIDENCE");
    d.once().await.unwrap();
    let tool: String =
        c.db.with(|c| {
            Ok(c.query_row(
                "SELECT body FROM context_sources WHERE kind='tool' LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .expect("observable tool evidence must be preserved");
    assert!(tool.contains("RAW-CANARY M artifact.txt (uncommitted)"));
    assert!(tool.contains("DO-NOT-REPLAY"));
    d.close(&Lane::new("writer", Some(7)), true).await.unwrap();
    mail.post(2, "continue");
    d.once().await.unwrap();
    let reply = mail.replies.lock().unwrap()[1].clone();
    assert!(
        !reply.contains("DO-NOT-REPLAY"),
        "raw tool input must only be accessible by explicit source retrieval"
    );
    assert!(reply.contains("Coverage gap"));
    assert!(reply.contains("read back unknown effects"));
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
}
#[tokio::test]
async fn capacity_rotation_is_between_turns_and_zero_disables_it() {
    for threshold in ["0", "64"] {
        let home = tempfile::tempdir().unwrap();
        let mut reg = Registry::default();
        reg.register(Box::new(Context)).unwrap();
        let c = Arc::new(
            Core::prepare(
                &reg,
                Db::open_in_memory().unwrap(),
                BootOptions {
                    env: vec![("AAF_CONTEXT_ROTATE_BYTES".into(), threshold.into())],
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let mail = Arc::new(Mail::default());
        let d = dispatcher(&c, mail.clone(), acp_roster(home.path(), false));
        let lane = Lane::new("writer", Some(7));
        mail.post(
            1,
            &format!("CAPACITY-CANARY next: verify {}", "padding ".repeat(100)),
        );
        d.once().await.unwrap();
        let first = mail.load_session(&lane).await.unwrap();
        mail.post(2, "continue");
        d.once().await.unwrap();
        let second = mail.load_session(&lane).await.unwrap();
        assert_eq!(
            first == second,
            threshold == "0",
            "threshold must control native session rotation"
        );
        assert_eq!(
            mail.replies.lock().unwrap()[1].contains("AAF CONTINUATION"),
            threshold != "0"
        );
        d.close(&lane, false).await.unwrap();
    }
}
#[tokio::test]
async fn accepted_steering_is_recorded_before_delivery_ack() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let mut r = acp_roster(home.path(), false);
    if let DriverSpec::Acp { acp_agent, .. } = &mut r.roles.get_mut("writer").unwrap().driver {
        *acp_agent = Some("hermes".into());
    }
    let d = dispatcher(&c, mail.clone(), r);
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "STEERABLE");
    d.tick().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if d.lane_driver(&lane).await.unwrap().status() == aaf_runtime::AgentStatus::Working {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    mail.post(2, "MID-TURN-CANARY next: read back artifact FINISH");
    d.once().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while d.busy(&lane) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let n:i64=c.db.with(|c|Ok(c.query_row("SELECT count(*) FROM context_sources WHERE kind='input' AND json_extract(body,'$.message')=2",[],|r|r.get(0))?)).unwrap();
    assert_eq!(
        n, 1,
        "steered requirements must enter canonical sources before ack"
    );
    assert!(mail.queued.lock().unwrap().is_empty());
    d.close(&lane, false).await.unwrap();
}
#[tokio::test]
async fn busy_switch_drains_old_executor_and_records_tool_gap_before_completion() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), acp_roster(home.path(), false));
    let lane = Lane::new("writer", Some(7));
    mail.post(
        1,
        "BUSY-CANARY next action: inspect dirty artifact NEED_PERMISSION",
    );
    d.tick().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let n: i64 =
                c.db.with(|c| {
                    Ok(c.query_row(
                        "SELECT count(*) FROM context_sources WHERE kind='tool'",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
            if n > 0
                && d.lane_driver(&lane).await.unwrap().status() == aaf_runtime::AgentStatus::Blocked
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let data: String =
        c.db.with(|c| {
            Ok(c.query_row(
                "SELECT body FROM context_sources WHERE kind='tool'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(data.contains("coverage gap: tool result not supplied"));
    assert!(
        mail.replies.lock().unwrap().is_empty(),
        "tool source is durable before turn ends"
    );
    let old = d.lane_driver(&lane).await.unwrap();
    mail.post(2, "queued while busy");
    d.apply_roster(roster(home.path())).await.unwrap();
    assert!(d.tick().await.unwrap().is_empty());
    assert_eq!(mail.queued.lock().unwrap().len(), 2);
    let request = old.pending_permissions().pop().unwrap();
    let aaf_runtime::DriverEvent::PermissionRequested { request_id, .. } = request else {
        panic!("permission request");
    };
    old.respond_permission(
        request_id,
        aaf_runtime::PermissionDecision::Selected {
            option_id: "allow".into(),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while d.busy(&lane) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    d.once().await.unwrap();
    assert_eq!(old.status(), aaf_runtime::AgentStatus::Offline);
    let reply = mail.replies.lock().unwrap().last().unwrap().clone();
    assert!(reply.contains("BUSY-CANARY"));
    assert!(reply.contains("queued while busy"));
    assert!(mail.queued.lock().unwrap().is_empty());
    d.close(&lane, false).await.unwrap();
}
#[tokio::test]
async fn force_closed_turn_still_checkpoints_and_honors_explicit_stop_ack() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let mut d =
        dispatcher(&c, mail.clone(), acp_roster(home.path(), false)).with_stop_notice(false);
    d.stop_grace = std::time::Duration::ZERO;
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "HANG");
    d.tick().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while d.lane_driver(&lane).await.unwrap().status() != aaf_runtime::AgentStatus::Working {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    d.cancel(&lane).await.unwrap();
    d.reap_stuck().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while d.busy(&lane) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        mail.queued.lock().unwrap().is_empty(),
        "explicit stop must not be replayed because its recorder was closed"
    );
    let n: i64 = c
        .db
        .with(|c| Ok(c.query_row("SELECT count(*) FROM context_checkpoints", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(n, 1);
}
#[tokio::test]
async fn failed_prompt_retry_gets_unknown_effects_recovery_without_losing_requirements() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let mut d = dispatcher(&c, mail.clone(), acp_roster(home.path(), false));
    d.retry_backoff = vec![std::time::Duration::ZERO];
    mail.post(1, "ERROR-CANARY next: read back artifact");
    d.once().await.unwrap();
    mail.post(2, "FAIL_ONCE");
    d.once().await.unwrap();
    assert_eq!(mail.queued.lock().unwrap().len(), 1);
    d.once().await.unwrap();
    let reply = mail.replies.lock().unwrap().last().unwrap().clone();
    assert!(
        reply.contains("ERROR-CANARY"),
        "retry after possible effects must receive recovery evidence"
    );
    assert!(reply.contains("read back unknown effects"));
    mail.post(3, "ordinary after retry");
    d.once().await.unwrap();
    assert!(!mail
        .replies
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("AAF CONTINUATION"));
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
}
struct FinalEvidence {
    tx: tokio::sync::broadcast::Sender<aaf_runtime::DriverEvent>,
    status: Mutex<aaf_runtime::AgentStatus>,
}
#[async_trait]
impl aaf_runtime::Driver for FinalEvidence {
    fn kind(&self) -> &'static str {
        "test"
    }
    async fn start(&self) -> aaf_runtime::driver::DResult<()> {
        *self.status.lock().unwrap() = aaf_runtime::AgentStatus::Idle;
        Ok(())
    }
    async fn prompt(&self, _: &str) -> aaf_runtime::driver::DResult<aaf_runtime::TurnResult> {
        Ok(aaf_runtime::TurnResult {
            text: "done".into(),
            stop_reason: "end_turn".into(),
        })
    }
    async fn cancel(&self) -> aaf_runtime::driver::DResult<()> {
        Ok(())
    }
    async fn respond_permission(
        &self,
        _: u64,
        _: aaf_runtime::PermissionDecision,
    ) -> aaf_runtime::driver::DResult<()> {
        Ok(())
    }
    fn status(&self) -> aaf_runtime::AgentStatus {
        *self.status.lock().unwrap()
    }
    fn events(&self) -> tokio::sync::broadcast::Receiver<aaf_runtime::DriverEvent> {
        self.tx.subscribe()
    }
    async fn stop(&self) -> aaf_runtime::driver::DResult<()> {
        let _ = self.tx.send(aaf_runtime::DriverEvent::ToolEvidence {
            data: serde_json::json!({"id":"last-event","output":"FINAL-STOP-ARTIFACT"}),
        });
        *self.status.lock().unwrap() = aaf_runtime::AgentStatus::Offline;
        Ok(())
    }
}

#[tokio::test]
async fn switching_flushes_final_old_executor_evidence_before_aborting_forwarder() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(FinalEvidence {
        tx: tokio::sync::broadcast::channel(16).0,
        status: Mutex::new(aaf_runtime::AgentStatus::Offline),
    });
    let d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_factory(move |_| Some(driver.clone()));
    mail.post(1, "work");
    d.once().await.unwrap();
    d.close(&Lane::new("writer", Some(7)), true).await.unwrap();
    let n:i64=c.db.with(|c|Ok(c.query_row("SELECT count(*) FROM context_sources WHERE kind='tool' AND body LIKE '%FINAL-STOP-ARTIFACT%'",[],|r|r.get(0))?)).unwrap();
    assert_eq!(n, 1, "stop/drain must not discard queued final evidence");
}
#[tokio::test]
async fn faulted_lane_does_not_starve_another_room() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), roster(home.path()));
    c.db.with(|c|{c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints WHEN NEW.room=7 BEGIN SELECT RAISE(ABORT,'room checkpoint failure'); END;")?;Ok(())}).unwrap();
    mail.post(1, "fault room");
    assert!(d.once().await.is_err());
    mail.queued.lock().unwrap().push(InboxMessage {
        id: 2,
        from: "owner".into(),
        body: "other room must proceed".into(),
        room: Some(8),
        thread_id: None,
    });
    let launched = d
        .tick()
        .await
        .expect("a latched fault must not block unrelated lanes");
    assert_eq!(launched, vec![Lane::new("writer", Some(8))]);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while d.busy(&Lane::new("writer", Some(8))) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        mail.queued
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![1]
    );
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
    d.close(&Lane::new("writer", Some(8)), false).await.unwrap();
}

#[tokio::test]
async fn blocked_lane_is_reported_once_and_cleared_by_recovery() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), roster(home.path()));
    let lane = Lane::new("writer", Some(7));
    c.db.with(|c|{c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints WHEN NEW.room=7 BEGIN SELECT RAISE(ABORT,'room checkpoint failure'); END;")?;Ok(())}).unwrap();
    mail.post(1, "fault room");
    assert!(d.once().await.is_err());
    assert!(
        d.blocked(&lane).await.is_some(),
        "faulted lane must be visible as stuck"
    );
    // tick 會丟掉被擋 lane 的錯誤；事件必須照樣發出，而且重複派工只發一次
    d.tick().await.unwrap();
    d.tick().await.unwrap();
    let blocked = || {
        c.bus
            .since(0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "runtime.lane.blocked" && e.payload["lane"]["room"] == 7)
            .count()
    };
    assert_eq!(blocked(), 1);
    c.db.with(|c| {
        c.execute_batch("DROP TRIGGER fail_checkpoint;")?;
        Ok(())
    })
    .unwrap();
    d.close(&lane, false).await.unwrap();
    assert_eq!(d.blocked(&lane).await, None);
}
