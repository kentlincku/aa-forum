// Adapter: relative includes only; reviewer inputs/assertions unchanged.
include!("continuity.rs");

#[tokio::test]
async fn review_park_must_not_clear_unknown_effects_fault() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let mut d = dispatcher(&c, mail.clone(), roster(home.path()));
    d.idle_park = std::time::Duration::ZERO;
    c.db.with(|c| { c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints BEGIN SELECT RAISE(ABORT,'fault'); END;")?; Ok(()) }).unwrap();
    mail.post(1, "perform external action");
    assert!(d.once().await.is_err());
    assert!(d.once().await.is_err());
    let before: i64 =
        c.db.with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM context_sources WHERE kind='output'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    let parked = d.park_idle().await.unwrap();
    assert!(d.once().await.is_err());
    let after: i64 =
        c.db.with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM context_sources WHERE kind='output'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    println!("parked={parked:?}; external-action executions {before}->{after}");
    assert_eq!(
        before, after,
        "automatic parking must not authorize unknown-effects retry"
    );
}

#[tokio::test]
async fn review_ordinary_checkpoint_fault_needs_recovery_after_native_load() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), acp_roster(home.path(), false));
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "normal successful first turn");
    d.once().await.unwrap();
    c.db.with(|c| { c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints BEGIN SELECT RAISE(ABORT,'fault'); END;")?; Ok(()) }).unwrap();
    mail.post(2, "external action on ordinary turn");
    assert!(d.once().await.is_err());
    let pending: bool =
        c.db.with(|c| Ok(c.query_row("SELECT pending FROM context_sessions", [], |r| r.get(0))?))
            .unwrap();
    println!("pending after ordinary post-execution failure={pending}");
    d.close(&lane, false).await.unwrap();
    c.db.with(|c| {
        c.execute_batch("DROP TRIGGER fail_checkpoint;")?;
        Ok(())
    })
    .unwrap();
    d.once().await.unwrap();
    let reply = mail.replies.lock().unwrap().last().unwrap().clone();
    d.close(&lane, false).await.unwrap();
    assert!(
        reply.contains("AAF CONTINUATION"),
        "unknown effects retry after native load lacks recovery: {reply}"
    );
}

#[tokio::test]
async fn review_prepare_failure_must_not_starve_unrelated_room() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), roster(home.path()));
    c.bus.publish("runtime.agent.tool_evidence",1,serde_json::json!({"lane":{"role":"writer","room":7},"event":{"type":"status","status":"idle"}})).unwrap();
    mail.post(1, "bad room");
    mail.queued.lock().unwrap().push(InboxMessage {
        id: 2,
        from: "owner".into(),
        body: "healthy room".into(),
        room: Some(8),
        thread_id: None,
    });
    for _ in 0..3 {
        assert!(d.tick().await.is_err());
    }
    let healthy = d.lane_driver(&Lane::new("writer", Some(8))).await;
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
    assert!(
        healthy.is_some(),
        "persistent room-7 source error prevents room-8 from ever launching"
    );
}

#[test]
fn review_crash_suffix_requires_recovery_on_loaded_session() {
    use aaf_runtime::continuity::Continuity;
    let c = core(Db::open_in_memory().unwrap());
    let l = aaf_feature_context::Ledger::new(c.db.clone());
    let lane = Lane::new("writer", Some(7));
    l.started(&lane, Some("native"), false).unwrap();
    let p = l.prepare(&lane, true, &[]).unwrap();
    l.complete(
        &lane,
        &p,
        &Ok(aaf_runtime::TurnResult {
            text: "done".into(),
            stop_reason: "end_turn".into(),
        }),
    )
    .unwrap();
    l.prepare(
        &lane,
        false,
        &[InboxMessage {
            id: 2,
            from: "owner".into(),
            body: "external action".into(),
            room: Some(7),
            thread_id: None,
        }],
    )
    .unwrap();
    c.bus.publish("runtime.agent.tool_evidence",1,serde_json::json!({"lane":{"role":"writer","room":7},"event":{"type":"tool_evidence","data":{"id":"t","output":"external write observed; no checkpoint"}}})).unwrap();
    // Crash before complete, then successful native load of the same session.
    l.started(&lane, Some("native"), true).unwrap();
    let retry = l.prepare(&lane, false, &[]).unwrap();
    assert!(
        !retry.text.is_empty(),
        "durable crash suffix indexed but never included in recovery"
    );
}

#[tokio::test]
async fn review_disabled_feature_must_not_persist_raw_secret_evidence() {
    let home = tempfile::tempdir().unwrap();
    let mut reg = Registry::default();
    reg.register(Box::new(Context)).unwrap();
    let c = Arc::new(
        Core::prepare(
            &reg,
            Db::open_in_memory().unwrap(),
            BootOptions {
                env: vec![("AAF_CONTEXT_ENABLED".into(), "0".into())],
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let driver = Arc::new(FinalEvidence {
        tx: tokio::sync::broadcast::channel(16).0,
        status: Mutex::new(aaf_runtime::AgentStatus::Offline),
    });
    let cloned = driver.clone();
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_factory(move |_| Some(cloned.clone()));
    mail.post(1, "work");
    d.once().await.unwrap();
    driver.tx.send(aaf_runtime::DriverEvent::ToolEvidence{data:serde_json::json!({"id":"environment","output":{"AAF_TOKEN":"DUMMY-ROLE-CREDENTIAL-CANARY","OPENAI_API_KEY":"DUMMY-PROVIDER-CREDENTIAL"}})}).unwrap();
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
    let persisted: i64 =
        c.db.with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM events WHERE payload LIKE '%DUMMY-ROLE-CREDENTIAL-CANARY%'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        persisted, 0,
        "raw credentials persisted even with AAF_CONTEXT_ENABLED=0"
    );
}

struct Burst {
    inner: FinalEvidence,
}
#[async_trait]
impl aaf_runtime::Driver for Burst {
    fn kind(&self) -> &'static str {
        "test"
    }
    async fn start(&self) -> aaf_runtime::driver::DResult<()> {
        aaf_runtime::Driver::start(&self.inner).await
    }
    async fn prompt(&self, _: &str) -> aaf_runtime::driver::DResult<aaf_runtime::TurnResult> {
        for _ in 0..64 {
            let _ = self
                .inner
                .tx
                .send(aaf_runtime::DriverEvent::MessageChunk { text: "x".into() });
        }
        Ok(aaf_runtime::TurnResult {
            text: "success".into(),
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
        *self.inner.status.lock().unwrap()
    }
    fn events(&self) -> tokio::sync::broadcast::Receiver<aaf_runtime::DriverEvent> {
        self.inner.tx.subscribe()
    }
    async fn stop(&self) -> aaf_runtime::driver::DResult<()> {
        Ok(())
    }
}
#[tokio::test]
async fn review_disabled_feature_preserves_legacy_lag_behavior() {
    let home = tempfile::tempdir().unwrap();
    let mut reg = Registry::default();
    reg.register(Box::new(Context)).unwrap();
    let c = Arc::new(
        Core::prepare(
            &reg,
            Db::open_in_memory().unwrap(),
            BootOptions {
                env: vec![("AAF_CONTEXT_ENABLED".into(), "0".into())],
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(Burst {
        inner: FinalEvidence {
            tx: tokio::sync::broadcast::channel(16).0,
            status: Mutex::new(aaf_runtime::AgentStatus::Offline),
        },
    });
    let d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_factory(move |_| Some(driver.clone()));
    mail.post(1, "work");
    let result = d.once().await;
    let _ = d.close(&Lane::new("writer", Some(7)), false).await;
    assert!(
        result.is_ok(),
        "off switch still enforces durable recording and blocks successful delivery: {result:?}"
    );
    assert!(mail.queued.lock().unwrap().is_empty());
}
