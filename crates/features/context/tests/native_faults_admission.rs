include!("native_faults.rs");

#[tokio::test]
async fn recorder_publish_or_observe_failure_blocks_even_if_fault_persistence_fails() {
    for publish in [false, true] {
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
        c.db.with(|db| {
            db.execute_batch("CREATE TRIGGER fail_fault BEFORE UPDATE OF fault ON context_sessions BEGIN SELECT RAISE(ABORT,'fault persistence unavailable'); END;")?;
            if publish {
                db.execute_batch("CREATE TRIGGER fail_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'event publication unavailable'); END;")?;
            }
            Ok(())
        }).unwrap();
        driver
            .inner
            .tx
            .send(aaf_runtime::DriverEvent::ToolEvidence {
                data: serde_json::json!({"output":"missing tool id"}),
            })
            .unwrap();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let lane = Lane::new("writer", Some(7));
        let ledger = aaf_feature_context::Ledger::new(c.db.clone());
        assert!(
            aaf_runtime::continuity::Continuity::fault(&ledger, &lane)
                .unwrap()
                .is_none(),
            "persistence was genuinely refused"
        );
        mail.post(2, "must stay queued");
        assert!(d.once().await.is_err());
        assert_eq!(driver.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(mail.queued.lock().unwrap().iter().any(|m| m.id == 2));
        c.db.with(|db| {
            db.execute_batch("DROP TRIGGER fail_fault; DROP TRIGGER IF EXISTS fail_event;")?;
            Ok(())
        })
        .unwrap();
        let _ = d.close(&lane, false).await;
        let _ = d.close(&lane, false).await;
    }
}

#[tokio::test]
async fn pending_idle_recorder_failure_blocks_without_scheduler_yield() {
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
