include!("continuity.rs");

#[tokio::test]
async fn teardown_recorder_fault_survives_removed_executor() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(FinalEvidence {
        tx: tokio::sync::broadcast::channel(16).0,
        status: Mutex::new(aaf_runtime::AgentStatus::Offline),
    });
    let cloned = driver.clone();
    let mut d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_factory(move |_| Some(cloned.clone()));
    d.idle_park = std::time::Duration::ZERO;
    mail.post(1, "completed turn");
    d.once().await.unwrap();
    driver
        .tx
        .send(aaf_runtime::DriverEvent::ToolEvidence {
            data: serde_json::json!({"output":"late unknown effect without id"}),
        })
        .unwrap();
    assert!(d.park_idle().await.is_err());
    assert!(
        aaf_runtime::continuity::Continuity::fault(
            &aaf_feature_context::Ledger::new(c.db.clone()),
            &Lane::new("writer", Some(7))
        )
        .unwrap()
        .is_some(),
        "removed recorder must leave a durable fault"
    );
    mail.post(2, "must stay blocked");
    assert!(
        d.once().await.is_err(),
        "teardown recorder error must persist after executor removal"
    );
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
}

#[test]
fn common_text_credentials_are_redacted() {
    let s = aaf_runtime::sanitize::Sanitizer::default();
    let safe=s.text("Authorization: Basic ZmFrZTpmYWtl; https://fake-user:fake-url-password@example.invalid/path\n-----BEGIN PRIVATE KEY-----\nFAKE-PEM-CONTENT\n-----END PRIVATE KEY-----\nkeep this evidence");
    for value in ["ZmFrZTpmYWtl", "fake-url-password", "FAKE-PEM-CONTENT"] {
        assert!(!safe.contains(value), "unredacted fake credential: {value}");
    }
    assert!(safe.contains("keep this evidence"));
}

#[test]
fn late_chunks_are_not_hidden_by_an_already_completed_attempt() {
    use aaf_runtime::{continuity::Continuity, DriverEvent, TurnResult};
    let c = core(Db::open_in_memory().unwrap());
    let l = aaf_feature_context::Ledger::new(c.db.clone());
    let lane = Lane::new("writer", Some(7));
    l.started(&lane, Some("one"), false).unwrap();
    let p = l.prepare(&lane, false, &[]).unwrap();
    l.complete(
        &lane,
        &p,
        &Ok(TurnResult {
            text: "final".into(),
            stop_reason: "end_turn".into(),
        }),
    )
    .unwrap();
    l.observed(
        &lane,
        100,
        &DriverEvent::MessageChunk {
            text: "LATE-UNCOMMITTED-EVIDENCE".into(),
        },
    )
    .unwrap();
    let p = l.prepare(&lane, true, &[]).unwrap();
    assert!(p.text.contains("LATE-UNCOMMITTED-EVIDENCE"));
}

#[tokio::test]
async fn started_failure_still_dispatches_healthy_lane() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), roster(home.path()));
    c.db.with(|c|{c.execute_batch("CREATE TRIGGER fail_start BEFORE INSERT ON context_sessions WHEN NEW.room=7 BEGIN SELECT RAISE(ABORT,'start storage failure'); END;")?;Ok(())}).unwrap();
    mail.post(1, "bad room");
    mail.queued.lock().unwrap().push(InboxMessage {
        id: 2,
        from: "owner".into(),
        body: "healthy room".into(),
        room: Some(8),
        thread_id: None,
    });
    assert!(d.once().await.is_err());
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

struct FinalThenRefuses {
    inner: FinalEvidence,
}
#[async_trait]
impl aaf_runtime::Driver for FinalThenRefuses {
    fn kind(&self) -> &'static str {
        "stop-refuses"
    }
    async fn start(&self) -> aaf_runtime::driver::DResult<()> {
        aaf_runtime::Driver::start(&self.inner).await
    }
    async fn prompt(&self, p: &str) -> aaf_runtime::driver::DResult<aaf_runtime::TurnResult> {
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
        self.inner.tx.subscribe()
    }
    async fn stop(&self) -> aaf_runtime::driver::DResult<()> {
        let _ = self.inner.tx.send(aaf_runtime::DriverEvent::ToolEvidence {
            data: serde_json::json!({"id":"stop-attempt","output":"FINAL-BUT-STILL-ALIVE"}),
        });
        Err(aaf_runtime::driver::DriverError::Protocol(
            "stop unverified".into(),
        ))
    }
}
#[tokio::test]
async fn failed_stop_retains_executor_and_final_evidence() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(FinalThenRefuses {
        inner: FinalEvidence {
            tx: tokio::sync::broadcast::channel(16).0,
            status: Mutex::new(aaf_runtime::AgentStatus::Offline),
        },
    });
    let old = driver.clone();
    let d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_factory(move |_| Some(driver.clone()));
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "work");
    d.once().await.unwrap();
    assert!(d.close(&lane, true).await.is_err());
    let registered = d.lane_driver(&lane).await.unwrap();
    assert!(Arc::ptr_eq(
        &registered,
        &(old.clone() as Arc<dyn aaf_runtime::Driver>)
    ));
    let count: i64 =
        c.db.with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM context_sources WHERE body LIKE '%FINAL-BUT-STILL-ALIVE%'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(count, 1);
    mail.post(2, "must not execute");
    assert!(d.once().await.is_err());
    assert_eq!(mail.replies.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn input_and_final_output_use_same_credential_policy() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_env_provider(|_| vec![("AAF_TOKEN".into(), "fake-known-turn-value".into())]);
    mail.post(1, "opaque fake-known-turn-value API_KEY=fake-turn-key");
    d.once().await.unwrap();
    let bodies: Vec<String> =
        c.db.with(|c| {
            let mut s = c.prepare("SELECT body FROM context_sources")?;
            let rows = s
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .unwrap();
    assert!(!bodies.join("\n").contains("fake-known-turn-value"));
    assert!(!bodies.join("\n").contains("fake-turn-key"));
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
}

#[test]
fn rotation_excludes_package_and_json_overhead() {
    use aaf_runtime::{continuity::Continuity, TurnResult};
    let c = core(Db::open_in_memory().unwrap());
    let mut l = aaf_feature_context::Ledger::configured(&c).unwrap();
    let lane = Lane::new("writer", Some(7));
    assert_eq!(
        l.settings.rotate_bytes, 0,
        "rotation must be opt-in without native capacity knowledge"
    );
    l.settings.rotate_bytes = 64;
    l.started(&lane, Some("one"), false).unwrap();
    let p = l
        .prepare(
            &lane,
            false,
            &[InboxMessage {
                id: 1,
                from: "owner".into(),
                body: "requirement".repeat(10),
                room: Some(7),
                thread_id: None,
            }],
        )
        .unwrap();
    l.complete(
        &lane,
        &p,
        &Ok(TurnResult {
            text: "OK".into(),
            stop_reason: "end_turn".into(),
        }),
    )
    .unwrap();
    assert!(l.rotate(&lane).unwrap());
    l.started(&lane, Some("two"), false).unwrap();
    let p = l
        .prepare(
            &lane,
            true,
            &[InboxMessage {
                id: 2,
                from: "owner".into(),
                body: "ok".into(),
                room: Some(7),
                thread_id: None,
            }],
        )
        .unwrap();
    assert!(p.text.len() > 64);
    l.complete(
        &lane,
        &p,
        &Ok(TurnResult {
            text: "OK".into(),
            stop_reason: "end_turn".into(),
        }),
    )
    .unwrap();
    assert!(!l.rotate(&lane).unwrap());
    let bytes: i64 =
        c.db.with(|c| Ok(c.query_row("SELECT bytes FROM context_sessions", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(bytes, 4);
    l.started(&lane, Some("three"), false).unwrap();
    l.settings.rotate_bytes = 262144;
    l.complete(
        &lane,
        &Default::default(),
        &Ok(TurnResult {
            text: "\n".repeat(131100),
            stop_reason: "end_turn".into(),
        }),
    )
    .unwrap();
    let bytes: i64 =
        c.db.with(|c| Ok(c.query_row("SELECT bytes FROM context_sessions", [], |r| r.get(0))?))
            .unwrap();
    assert_eq!(bytes, 131100);
    assert!(!l.rotate(&lane).unwrap());
}

mod logical_http {
    include!("http.rs");
    #[tokio::test]
    async fn fault_diagnostic_and_owner_explicit_recovery_api() {
        use aaf_runtime::{continuity::Continuity, dispatcher::Lane};
        let home = tempfile::tempdir().unwrap();
        let chat = Arc::new(aaf_feature_chat::Chat::new(super::roster(home.path())));
        chat.set_continuity(aaf_feature_context::runtime);
        struct Wrap(Arc<aaf_feature_chat::Chat>);
        impl aaf_core::Feature for Wrap {
            fn name(&self) -> &'static str {
                aaf_core::Feature::name(self.0.as_ref())
            }
            fn migrations(&self) -> &'static [aaf_core::storage::Migration] {
                aaf_core::Feature::migrations(self.0.as_ref())
            }
            fn capabilities(&self) -> &'static [aaf_core::perms::CapabilityDef] {
                aaf_core::Feature::capabilities(self.0.as_ref())
            }
            fn config(&self) -> &'static [aaf_core::config::ConfigKey] {
                aaf_core::Feature::config(self.0.as_ref())
            }
        }
        let mut reg = Registry::default();
        reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
        reg.register(Box::new(Wrap(chat.clone()))).unwrap();
        reg.register(Box::new(Context)).unwrap();
        let c = Arc::new(
            Core::prepare(&reg, Db::open_in_memory().unwrap(), BootOptions::default()).unwrap(),
        );
        c.db.with(|c| {
            c.execute(
                "INSERT INTO chat_rooms(id,name,created_by,created_at) VALUES(7,'test','owner',0)",
                [],
            )?;
            c.execute(
                "INSERT INTO chat_members(room,member,kind,added_at) VALUES(7,'writer','role',0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let app = aaf_api::app(
            c.clone(),
            &[
                chat,
                Arc::new(aaf_feature_context::http::ContextHttp::new(Arc::new(
                    |_, _, _| Ok(true),
                ))),
            ],
        );
        let setup = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/setup")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        json!({"name":"owner","password":"test-only-password"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(setup.status(), 200);
        let owner: Value =
            serde_json::from_slice(&to_bytes(setup.into_body(), 10000).await.unwrap()).unwrap();
        let owner = owner["token"].as_str().unwrap();
        let l = aaf_feature_context::Ledger::new(c.db.clone());
        let lane = Lane::new("writer", Some(7));
        l.started(&lane, Some("s"), false).unwrap();
        l.prepare(&lane, false, &[]).unwrap();
        l.failed(&lane, "test storage failure").unwrap();
        let token = aaf_api::issue_role_token(&c, "writer", "lead").unwrap();
        let root = "/api/v1/context/rooms/7/roles/writer/sources";
        let (_, v) = get(&app, owner, root).await;
        assert_eq!(v["diagnostic"]["blocked"], true);
        assert!(v["diagnostic"]["recovery"]
            .as_str()
            .unwrap()
            .contains("/api/v1/chat/rooms/7/lanes/writer/close?forget=false"));
        let close = "/api/v1/chat/rooms/7/lanes/writer/close?forget=false";
        for (token, status) in [(&token[..], 403), (owner, 200)] {
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(close)
                        .header("Authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), status);
            if status == 403 {
                assert!(l.fault(&lane).unwrap().is_some());
            }
        }
        assert!(l.fault(&lane).unwrap().is_none());
        let (_, v) = get(&app, owner, root).await;
        assert_eq!(v["diagnostic"]["blocked"], false);
        assert_eq!(v["diagnostic"]["pending"], true);
    }

    #[tokio::test]
    async fn completed_token_chunks_do_not_crowd_out_logical_records() {
        use aaf_runtime::{
            continuity::Continuity,
            dispatcher::{InboxMessage, Lane},
            DriverEvent, TurnResult,
        };
        let mut reg = Registry::default();
        reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
        reg.register(Box::new(Context)).unwrap();
        let c = Arc::new(
            Core::prepare(&reg, Db::open_in_memory().unwrap(), BootOptions::default()).unwrap(),
        );
        let l = aaf_feature_context::Ledger::new(c.db.clone());
        let lane = Lane::new("writer", Some(7));
        l.started(&lane, Some("one"), false).unwrap();
        for n in 1..=2 {
            let p = l
                .prepare(
                    &lane,
                    false,
                    &[InboxMessage {
                        id: n,
                        from: "owner".into(),
                        body: format!("COMPLETE-REQUEST-{n}"),
                        room: Some(7),
                        thread_id: None,
                    }],
                )
                .unwrap();
            for i in 0..140 {
                l.observed(
                    &lane,
                    (n * 1000 + i) as u64,
                    &DriverEvent::MessageChunk { text: "x".into() },
                )
                .unwrap();
            }
            l.complete(
                &lane,
                &p,
                &Ok(TurnResult {
                    text: format!("COMPLETE-RESULT-{n}"),
                    stop_reason: "end_turn".into(),
                }),
            )
            .unwrap();
        }
        let p = l.prepare(&lane, true, &[]).unwrap();
        for expected in [
            "COMPLETE-REQUEST-1",
            "COMPLETE-RESULT-1",
            "COMPLETE-REQUEST-2",
            "COMPLETE-RESULT-2",
        ] {
            assert!(p.text.contains(expected), "lost logical record: {expected}");
        }
        let token = aaf_api::issue_role_token(&c, "writer", "worker").unwrap();
        let app = aaf_api::app(
            c.clone(),
            &[Arc::new(aaf_feature_context::http::ContextHttp::new(
                Arc::new(|_, _, _| Ok(true)),
            ))],
        );
        let root = "/api/v1/context/rooms/7/roles/writer/sources";
        let (status, v) = get(&app, &token, root).await;
        assert_eq!(status, 200);
        assert_eq!(v["sources"].as_array().unwrap().len(), 4);
        assert!(v["next_after"].is_null());
        let raw: i64 =
            c.db.with(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM context_sources WHERE kind='assistant_chunk'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(raw, 280);
        // An unfinished stream is readable as one aggregate, with every raw id retained.
        for (i, text) in ["PARTIAL-", "READABLE-", "RESPONSE"]
            .into_iter()
            .enumerate()
        {
            l.observed(
                &lane,
                9000 + i as u64,
                &DriverEvent::MessageChunk { text: text.into() },
            )
            .unwrap();
        }
        let (_, v) = get(&app, &token, root).await;
        let part = v["sources"].as_array().unwrap().last().unwrap();
        assert_eq!(part["kind"], "assistant_partial");
        assert!(part["excerpt"]
            .as_str()
            .unwrap()
            .contains("PARTIAL-READABLE-RESPONSE"));
        let (_, full) = get(&app, &token, &format!("{root}?source={}", part["id"])).await;
        assert!(full["text"].as_str().unwrap().contains("source_ids"));
        let (status, raw) = get(&app, &token, &format!("{root}?raw=true&limit=100")).await;
        assert_eq!(status, 200);
        assert_eq!(raw["sources"].as_array().unwrap().len(), 100);
    }
}

#[test]
fn continuation_prioritizes_current_request_and_minimum_budget_sources() {
    use aaf_runtime::continuity::Continuity;
    let c = core(Db::open_in_memory().unwrap());
    c.db.with(|c| {
        for i in 0..20 {
            c.execute(
                "INSERT INTO context_sources(role,room,kind,body) VALUES('writer',7,'input',?1)",
                [serde_json::json!({"body":format!("requirement-{i}")}).to_string()],
            )?;
        }
        Ok(())
    })
    .unwrap();
    let mut l = aaf_feature_context::Ledger::configured(&c).unwrap();
    l.settings.budget = 1024;
    let p = l.prepare(&Lane::new("writer", Some(7)), true, &[]).unwrap();
    assert!(p.text.contains("current request takes priority"));
    assert!(p.text.contains("tools are permitted"));
    assert!(!p.text.contains("Read full earlier decisions"));
    assert!(p.text.len() <= 1024);
    assert!(
        p.text.lines().any(|l| l.starts_with('{')),
        "minimum budget must retain a usable source"
    );
    assert!(
        p.text.contains("requirement-19"),
        "latest requirement must remain usable at minimum budget"
    );
}

#[tokio::test]
async fn durable_fault_blocks_restarted_dispatcher_until_explicit_close() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let d = dispatcher(&c, mail.clone(), roster(home.path()));
    mail.post(1, "side effect");
    c.db.with(|c| { c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints BEGIN SELECT RAISE(ABORT,'fault'); END;")?; Ok(()) }).unwrap();
    assert!(d.once().await.is_err());
    c.db.with(|c| {
        c.execute_batch("DROP TRIGGER fail_checkpoint;")?;
        Ok(())
    })
    .unwrap();
    // New scheduler sharing only durable storage, not the old in-memory latch.
    let restarted = dispatcher(&c, mail.clone(), roster(home.path()));
    assert!(
        restarted.once().await.is_err(),
        "restart must not retry unknown effects"
    );
    assert_eq!(mail.queued.lock().unwrap().len(), 1);
    restarted
        .close(&Lane::new("writer", Some(7)), false)
        .await
        .unwrap();
    restarted.once().await.unwrap();
    assert!(mail
        .replies
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("AAF CONTINUATION"));
    restarted
        .close(&Lane::new("writer", Some(7)), false)
        .await
        .unwrap();
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
}

#[tokio::test]
async fn evidence_is_sanitized_before_event_and_source_persistence() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(FinalEvidence {
        tx: tokio::sync::broadcast::channel(32).0,
        status: Mutex::new(aaf_runtime::AgentStatus::Offline),
    });
    let cloned = driver.clone();
    let d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_factory(move |_| Some(cloned.clone()))
        .with_env_provider(|_| vec![("AAF_TOKEN".into(), "fake-known-opaque-value".into())]);
    mail.post(1, "work");
    d.once().await.unwrap();
    driver.tx.send(aaf_runtime::DriverEvent::ToolEvidence { data: serde_json::json!({"id":"safe-id", "output":{"AAF_TOKEN":"fake-aaf-secret", "OPENAI_API_KEY":"fake-provider-secret", "password":"fake-password", "nested":[{"Authorization":"Bearer fake-bearer"}], "text":"opaque: fake-known-opaque-value; API_KEY=fake-text-key password: fake-text-pass Authorization: Bearer fake-text-auth"}}) }).unwrap();
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
    let texts: Vec<String> =
        c.db.with(|c| {
            let mut s =
                c.prepare("SELECT payload FROM events UNION ALL SELECT body FROM context_sources")?;
            let rows = s
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .unwrap();
    let all = texts.join("\n");
    for secret in [
        "fake-known-opaque-value",
        "fake-aaf-secret",
        "fake-provider-secret",
        "fake-password",
        "fake-bearer",
        "fake-text-key",
        "fake-text-pass",
        "fake-text-auth",
    ] {
        assert!(!all.contains(secret), "persisted fake credential: {secret}");
    }
    assert!(all.contains("safe-id"));
    assert!(all.contains("[REDACTED]"));
}
