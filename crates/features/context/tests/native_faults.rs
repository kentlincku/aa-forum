// Recorder-fault and native process-group regression tests (the native one is ignored by default;
// fixtures/native_faults/run_probes.py supplies the required DYLD injector).
include!("continuity.rs");

struct CountCalls {
    inner: FinalEvidence,
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl aaf_runtime::Driver for CountCalls {
    fn kind(&self) -> &'static str {
        "native_faults"
    }
    async fn start(&self) -> aaf_runtime::driver::DResult<()> {
        aaf_runtime::Driver::start(&self.inner).await
    }
    async fn prompt(&self, p: &str) -> aaf_runtime::driver::DResult<aaf_runtime::TurnResult> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        aaf_runtime::Driver::prompt(&self.inner, p).await
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
        aaf_runtime::Driver::status(&self.inner)
    }
    fn events(&self) -> tokio::sync::broadcast::Receiver<aaf_runtime::DriverEvent> {
        aaf_runtime::Driver::events(&self.inner)
    }
    async fn stop(&self) -> aaf_runtime::driver::DResult<()> {
        aaf_runtime::Driver::stop(&self.inner).await
    }
}
#[tokio::test]
async fn native_idle_recorder_fault_blocks_before_next_effect() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(CountCalls {
        inner: FinalEvidence {
            tx: tokio::sync::broadcast::channel(16).0,
            status: Mutex::new(aaf_runtime::AgentStatus::Offline),
        },
        calls: Default::default(),
    });
    let cloned = driver.clone();
    let d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_factory(move |_| Some(cloned.clone()));
    mail.post(1, "first action");
    d.once().await.unwrap();
    // Idle/background observations overflow the recorder after a successful checkpoint.
    for _ in 0..64 {
        driver
            .inner
            .tx
            .send(aaf_runtime::DriverEvent::MessageChunk {
                text: "background effect".into(),
            })
            .unwrap();
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let ledger = aaf_feature_context::Ledger::new(c.db.clone());
    let before =
        aaf_runtime::continuity::Continuity::fault(&ledger, &Lane::new("writer", Some(7))).unwrap();
    mail.post(2, "must not execute after known coverage failure");
    let next = d.once().await;
    let calls = driver.calls.load(std::sync::atomic::Ordering::SeqCst);
    println!("IDLE_GAP durable_fault_before={before:?} next={next:?} prompt_calls={calls}");
    let _ = d.close(&Lane::new("writer", Some(7)), false).await;
    assert_eq!(
        calls, 1,
        "known recorder coverage fault must block before another external-effect turn"
    );
}

struct SteeringGap {
    inner: FinalEvidence,
    release: tokio::sync::Notify,
    emit_gap: bool,
}
#[async_trait]
impl aaf_runtime::Driver for SteeringGap {
    fn kind(&self) -> &'static str {
        "steering-gap"
    }
    async fn start(&self) -> aaf_runtime::driver::DResult<()> {
        aaf_runtime::Driver::start(&self.inner).await
    }
    async fn prompt(&self, _: &str) -> aaf_runtime::driver::DResult<aaf_runtime::TurnResult> {
        *self.inner.status.lock().unwrap() = aaf_runtime::AgentStatus::Working;
        self.release.notified().await;
        *self.inner.status.lock().unwrap() = aaf_runtime::AgentStatus::Idle;
        Ok(aaf_runtime::TurnResult {
            text: "main done".into(),
            stop_reason: "end_turn".into(),
        })
    }
    async fn steer(&self, _: &str) -> aaf_runtime::driver::DResult<aaf_runtime::SteerOutcome> {
        if self.emit_gap {
            self.inner
                .tx
                .send(aaf_runtime::DriverEvent::ToolEvidence {
                    data: serde_json::json!({"output":"unknown steer effect, missing tool id"}),
                })
                .unwrap();
        }
        Ok(aaf_runtime::SteerOutcome::Steered)
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
        aaf_runtime::Driver::status(&self.inner)
    }
    fn events(&self) -> tokio::sync::broadcast::Receiver<aaf_runtime::DriverEvent> {
        aaf_runtime::Driver::events(&self.inner)
    }
    async fn stop(&self) -> aaf_runtime::driver::DResult<()> {
        aaf_runtime::Driver::stop(&self.inner).await
    }
}
#[tokio::test]
async fn native_steering_recorder_failure_must_not_ack() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(SteeringGap {
        inner: FinalEvidence {
            tx: tokio::sync::broadcast::channel(16).0,
            status: Mutex::new(aaf_runtime::AgentStatus::Offline),
        },
        release: Default::default(),
        emit_gap: true,
    });
    let mut r = acp_roster(home.path(), false);
    if let DriverSpec::Acp { acp_agent, .. } = &mut r.roles.get_mut("writer").unwrap().driver {
        *acp_agent = Some("hermes".into());
    }
    let cloned = driver.clone();
    let d = dispatcher(&c, mail.clone(), r).with_factory(move |_| Some(cloned.clone()));
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "main action");
    d.tick().await.unwrap();
    while d.lane_driver(&lane).await.unwrap().status() != aaf_runtime::AgentStatus::Working {
        tokio::task::yield_now().await;
    }
    mail.post(2, "steering requirement");
    let result = d.once().await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let queued: Vec<_> = mail.queued.lock().unwrap().iter().map(|m| m.id).collect();
    driver.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while d.busy(&lane) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let fault = aaf_runtime::continuity::Continuity::fault(
        &aaf_feature_context::Ledger::new(c.db.clone()),
        &lane,
    )
    .unwrap();
    println!("STEERING_GAP result={result:?} queued_after_steer={queued:?} final_fault={fault:?}");
    let _ = d.close(&lane, false).await;
    let _ = d.close(&lane, false).await;
    assert!(
        queued.contains(&2),
        "steering delivery must remain queued when its recorder cannot commit evidence"
    );
}

#[tokio::test]
async fn native_steering_rotation_counts_input_once() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(SteeringGap {
        inner: FinalEvidence {
            tx: tokio::sync::broadcast::channel(16).0,
            status: Mutex::new(aaf_runtime::AgentStatus::Offline),
        },
        release: Default::default(),
        emit_gap: false,
    });
    let mut r = acp_roster(home.path(), false);
    if let DriverSpec::Acp { acp_agent, .. } = &mut r.roles.get_mut("writer").unwrap().driver {
        *acp_agent = Some("hermes".into());
    }
    let cloned = driver.clone();
    let d = dispatcher(&c, mail.clone(), r).with_factory(move |_| Some(cloned.clone()));
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "main action");
    d.tick().await.unwrap();
    while d.lane_driver(&lane).await.unwrap().status() != aaf_runtime::AgentStatus::Working {
        tokio::task::yield_now().await;
    }
    mail.post(2, "steering requirement");
    d.once().await.unwrap();
    driver.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while d.busy(&lane) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let actual: i64 =
        c.db.with(|c| Ok(c.query_row("SELECT bytes FROM context_sessions", [], |r| r.get(0))?))
            .unwrap();
    let expected = "main action".len() + "steering requirement".len() + "main done".len();
    println!("STEERING_BYTES actual={actual} expected={expected}");
    d.close(&lane, false).await.unwrap();
    assert_eq!(
        actual as usize, expected,
        "observed-text rotation must count steering input, not subtract an uncounted continuation"
    );
}

#[tokio::test]
#[ignore = "requires native DYLD fault injector; run fixtures/native_faults/run_probes.py"]
async fn native_native_acp_group_failure_refuses_switch() {
    assert!(
        std::env::var_os("AAF_REVIEW_DENY_GROUP").is_some(),
        "run with scoped fault injector"
    );
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let mut r = roster(home.path());
    r.roles.get_mut("writer").unwrap().driver = DriverSpec::Acp {
        command: vec![
            "/usr/bin/python3".into(),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/native_faults/fake_tree.py"
            )
            .into(),
            home.path().display().to_string(),
        ],
        acp_agent: None,
        model: None,
        mode: None,
        cwd: None,
        env: vec![],
        effort: None,
        account: None,
        transport: None,
    };
    let d = dispatcher(&c, mail.clone(), r);
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "fixture");
    d.once().await.unwrap();
    let child: u32 = std::fs::read_to_string(home.path().join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let birth = aaf_runtime::process::start_time(child).expect("owned inert child live");
    let old = d.lane_driver(&lane).await.unwrap();
    let switched = d.apply_roster(roster(home.path())).await;
    let child_still_live = aaf_runtime::process::start_time(child) == Some(birth);
    let registered = d.lane_driver(&lane).await.is_some();
    println!("NATIVE_GROUP_FAILURE switch={switched:?} old_status={:?} registered_old={registered} child_same_identity_alive={child_still_live}",old.status());
    // Cooperative bounded cleanup, never signal a reused/reaped group.
    std::fs::write(home.path().join("release"), "release owned inert fixture").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    println!(
        "NATIVE_CLEANUP child_birth_after={:?}",
        aaf_runtime::process::start_time(child)
    );
    let _ = d.close(&lane, false).await;
    assert!(!child_still_live || switched.is_err(),"native ACP must not report successful replacement while its owned child survived a failed group signal");
}
