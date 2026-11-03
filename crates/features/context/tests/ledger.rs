use aaf_core::{storage::Db, Feature};
use aaf_feature_context::Context;

#[test]
fn canonical_sources_and_versioned_checkpoints_survive_reopen() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("context.sqlite3");
    {
        let db = Db::open(&path).unwrap();
        db.migrate("context", Context.migrations()).unwrap();
        db.with(|c| {
            c.execute("INSERT INTO context_sources(role,room,kind,body,source_key) VALUES ('writer',7,'input','{\"body\":\"CANARY next: verify artifact\"}','message:1')", [])?;
            c.execute("INSERT INTO context_checkpoints(role,room,cursor,status) VALUES ('writer',7,1,'end_turn')", [])?;
            c.execute("INSERT INTO context_checkpoints(role,room,cursor,status) VALUES ('writer',7,1,'end_turn')", [])?;
            Ok(())
        }).expect("canonical sources and immutable checkpoint versions must exist");
    }
    let db = Db::open(&path).unwrap();
    db.with(|c| {
        let text: String = c.query_row(
            "SELECT body FROM context_sources WHERE role='writer' AND room=7",
            [],
            |r| r.get(0),
        )?;
        assert!(text.contains("CANARY"));
        let versions: i64 =
            c.query_row("SELECT count(*) FROM context_checkpoints", [], |r| r.get(0))?;
        assert_eq!(versions, 2);
        Ok(())
    })
    .unwrap();
}
#[test]
fn continuation_is_utf8_budgeted_and_marks_omitted_sources() {
    use aaf_runtime::{continuity::Continuity, dispatcher::Lane};
    let db = Db::open_in_memory().unwrap();
    db.migrate("context", Context.migrations()).unwrap();
    db.with(|c| {
        for i in 0..700 {
            c.execute(
                "INSERT INTO context_sources(role,room,kind,body) VALUES ('writer',7,'input',?1)",
                [
                    serde_json::json!({"body":format!("source {i} {}", "決策保留𠮷".repeat(100))})
                        .to_string(),
                ],
            )?;
        }
        Ok(())
    })
    .unwrap();
    let ledger = aaf_feature_context::Ledger::new(db);
    let p = ledger
        .prepare(&Lane::new("writer", Some(7)), true, &[])
        .unwrap();
    assert!(
        p.text.len() <= 8192,
        "must enforce conservative byte/token budget, got {}",
        p.text.len()
    );
    assert!(p.text.contains("aaf context read"));
    assert!(p.text.contains("omitted"));
    assert!(p.text.contains("source 0"));
    assert!(p.text.contains("source 699"));
}
fn configured(core: &aaf_core::Core) -> aaf_core::Result<aaf_feature_context::Ledger> {
    aaf_feature_context::Ledger::configured(core)
}

#[test]
fn all_config_knobs_validate_and_zero_disables_injection_or_recording() {
    use aaf_runtime::{
        continuity::Continuity,
        dispatcher::{InboxMessage, Lane},
    };
    for (key, value, want_budget, want_records, valid) in [
        ("AAF_CONTEXT_ENABLED", "0", 0, false, true),
        ("AAF_CONTEXT_BYTES", "0", 0, true, true),
        ("AAF_CONTEXT_TOKENS", "0", 0, true, true),
        ("AAF_CONTEXT_BYTES", "2048", 2048, true, true),
        ("AAF_CONTEXT_TOKENS", "3072", 3072, true, true),
        ("AAF_CONTEXT_ROTATE_BYTES", "0", 8192, true, true),
        ("AAF_CONTEXT_ROTATE_BYTES", "100", 8192, true, true),
        ("AAF_CONTEXT_PAGE_BYTES", "4", 8192, true, true),
        ("AAF_CONTEXT_ENABLED", "2", 0, false, false),
        ("AAF_CONTEXT_BYTES", "100", 0, false, false),
        ("AAF_CONTEXT_TOKENS", "-1", 0, false, false),
        ("AAF_CONTEXT_ROTATE_BYTES", "bad", 0, false, false),
        ("AAF_CONTEXT_PAGE_BYTES", "0", 0, false, false),
        ("AAF_CONTEXT_PAGE_BYTES", "1048577", 0, false, false),
    ] {
        let mut reg = aaf_core::Registry::default();
        reg.register(Box::new(Context)).unwrap();
        let c = aaf_core::Core::prepare(
            &reg,
            Db::open_in_memory().unwrap(),
            aaf_core::feature::BootOptions {
                env: vec![(key.into(), value.into())],
                ..Default::default()
            },
        )
        .unwrap();
        c.db.with(|c| {
            c.execute(
                "INSERT INTO context_sources(role,room,kind,body) VALUES ('writer',7,'input',?1)",
                [serde_json::json!({"body":"決策".repeat(10000)}).to_string()],
            )?;
            Ok(())
        })
        .unwrap();
        let l = configured(&c);
        if !valid {
            assert!(l.is_err(), "must reject {key}={value}");
            continue;
        }
        let p = l
            .unwrap()
            .prepare(
                &Lane::new("writer", Some(7)),
                true,
                &[InboxMessage {
                    id: 77,
                    from: "owner".into(),
                    body: "new".into(),
                    room: Some(7),
                    thread_id: None,
                }],
            )
            .unwrap();
        assert!(
            p.text.len() <= want_budget,
            "{key}={value}: {}",
            p.text.len()
        );
        let n: i64 = c
            .db
            .with(|c| Ok(c.query_row("SELECT count(*) FROM context_sources", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, if want_records { 2 } else { 1 }, "{key}={value}");
    }
}
#[test]
fn crash_suffix_catches_up_committed_tool_events_and_failed_checkpoint_output() {
    use aaf_runtime::{continuity::Continuity, dispatcher::Lane, DriverEvent, TurnResult};
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("state.db");
    {
        let db = Db::open(&path).unwrap();
        db.migrate("context", Context.migrations()).unwrap();
        let bus = aaf_core::events::EventBus::new(db, 16);
        // Simulate a crash after EventBus persisted an adapter observation but before indexing it.
        bus.publish("runtime.agent.tool_evidence",1,serde_json::json!({"lane":{"role":"writer","room":7},"event":DriverEvent::ToolEvidence {data:serde_json::json!({"id":"crash-tool","output":{"stdout":"DIRTY-ARTIFACT-CANARY"},"coverage":"supplied; effects unverified"})}})).unwrap();
    }
    let db = Db::open(&path).unwrap();
    let ledger = aaf_feature_context::Ledger::new(db.clone());
    let lane = Lane::new("writer", Some(7));
    let prepared = ledger.prepare(&lane, true, &[]).unwrap();
    let tools: i64 = db
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM context_sources WHERE kind='tool'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        tools, 1,
        "committed event suffix must not be skipped on restart"
    );
    db.with(|c|{c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints BEGIN SELECT RAISE(ABORT,'checkpoint failed'); END;")?;Ok(())}).unwrap();
    assert!(ledger
        .complete(
            &lane,
            &prepared,
            &Ok(TurnResult {
                text: "ARTIFACT-RESULT next: inspect uncommitted file".into(),
                stop_reason: "end_turn".into()
            })
        )
        .is_err());
    let body: String = db
        .with(|c| {
            Ok(c.query_row(
                "SELECT body FROM context_sources WHERE kind='output'",
                [],
                |r| r.get(0),
            )?)
        })
        .expect("observed result must survive a failed checkpoint");
    assert!(body.contains("ARTIFACT-RESULT"));
    assert!(ledger
        .prepare(&lane, true, &[])
        .unwrap()
        .text
        .contains("ARTIFACT-RESULT"));
}
#[test]
fn edited_delivery_is_a_new_source_version_not_an_overwrite_or_silent_ignore() {
    use aaf_runtime::{
        continuity::Continuity,
        dispatcher::{InboxMessage, Lane},
    };
    let db = Db::open_in_memory().unwrap();
    db.migrate("context", Context.migrations()).unwrap();
    let l = aaf_feature_context::Ledger::new(db.clone());
    let lane = Lane::new("writer", Some(7));
    let mut msg = InboxMessage {
        id: 1,
        from: "owner".into(),
        body: "decision A".into(),
        room: Some(7),
        thread_id: None,
    };
    l.prepare(&lane, false, std::slice::from_ref(&msg)).unwrap();
    msg.body = "decision B supersedes A".into();
    l.prepare(&lane, false, std::slice::from_ref(&msg)).unwrap();
    l.prepare(&lane, false, &[msg]).unwrap();
    db.with(|c| {
        let n: i64 = c.query_row("SELECT count(*) FROM context_sources", [], |r| r.get(0))?;
        assert_eq!(
            n, 2,
            "changed delivery text must append a version; identical retry must deduplicate"
        );
        Ok(())
    })
    .unwrap();
    let p = l.prepare(&lane, true, &[]).unwrap();
    assert!(p.text.contains("decision A"));
    assert!(p.text.contains("decision B"));
}
#[test]
fn rejects_cross_room_batches_and_preserves_other_lane_isolation() {
    use aaf_runtime::{
        continuity::Continuity,
        dispatcher::{InboxMessage, Lane},
    };
    let db = Db::open_in_memory().unwrap();
    db.migrate("context", Context.migrations()).unwrap();
    let l = aaf_feature_context::Ledger::new(db.clone());
    let lane = Lane::new("writer", Some(7));
    let msg = InboxMessage {
        id: 1,
        from: "owner".into(),
        body: "CROSS-ROOM-SECRET".into(),
        room: Some(8),
        thread_id: None,
    };
    assert!(
        l.prepare(&lane, false, std::slice::from_ref(&msg)).is_err(),
        "mixed-room input must fail closed"
    );
    l.prepare(&Lane::new("writer", Some(8)), false, &[msg])
        .unwrap();
    assert!(!l
        .prepare(&lane, true, &[])
        .unwrap()
        .text
        .contains("CROSS-ROOM-SECRET"));
    assert!(!l
        .prepare(&Lane::new("reviewer", Some(8)), true, &[])
        .unwrap()
        .text
        .contains("CROSS-ROOM-SECRET"));
    assert!(l.prepare(&Lane::new("writer", None), true, &[]).is_err());
}
#[test]
fn malformed_tool_event_fails_closed_instead_of_looping_forever() {
    use aaf_runtime::{continuity::Continuity, dispatcher::Lane};
    let db = Db::open_in_memory().unwrap();
    db.migrate("context", Context.migrations()).unwrap();
    let bus = aaf_core::events::EventBus::new(db.clone(), 16);
    bus.publish("runtime.agent.tool_evidence",1,serde_json::json!({"lane":{"role":"writer","room":7},"event":{"type":"status","status":"idle"}})).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let l = aaf_feature_context::Ledger::new(db);
        let _ = tx.send(l.prepare(&Lane::new("writer", Some(7)), true, &[]).is_err());
    });
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).ok(),
        Some(true),
        "wrong tool schema must fail closed, not spin forever"
    );
}
#[test]
fn injection_off_keeps_history_without_claiming_a_committed_injection() {
    use aaf_runtime::{
        continuity::Continuity,
        dispatcher::{InboxMessage, Lane},
        TurnResult,
    };
    let mut reg = aaf_core::Registry::default();
    reg.register(Box::new(Context)).unwrap();
    let c = aaf_core::Core::prepare(
        &reg,
        Db::open_in_memory().unwrap(),
        aaf_core::feature::BootOptions {
            env: vec![("AAF_CONTEXT_BYTES".into(), "0".into())],
            ..Default::default()
        },
    )
    .unwrap();
    let l = aaf_feature_context::Ledger::configured(&c).unwrap();
    let lane = Lane::new("writer", Some(7));
    l.prepare(
        &lane,
        false,
        &[InboxMessage {
            id: 1,
            from: "owner".into(),
            body: "retained canary".into(),
            room: Some(7),
            thread_id: None,
        }],
    )
    .unwrap();
    l.started(&lane, Some("new-session"), false).unwrap();
    let p = l.prepare(&lane, true, &[]).unwrap();
    assert!(p.text.is_empty());
    l.complete(
        &lane,
        &p,
        &Ok(TurnResult {
            text: "done".into(),
            stop_reason: "end_turn".into(),
        }),
    )
    .unwrap();
    let injected: i64 =
        c.db.with(|c| {
            Ok(
                c.query_row("SELECT injected_version FROM context_sessions", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(
        injected, 0,
        "checkpoint commit is not an injection when the package is disabled"
    );
}
#[test]
fn partial_assistant_stream_survives_crash_without_exposing_private_thoughts() {
    use aaf_runtime::{continuity::Continuity, dispatcher::Lane};
    let db = Db::open_in_memory().unwrap();
    db.migrate("context", Context.migrations()).unwrap();
    let bus = aaf_core::events::EventBus::new(db.clone(), 16);
    for (kind, text) in [
        ("message_chunk", "PARTIAL-CANARY artifact still unverified"),
        ("thought_chunk", "PRIVATE-THOUGHT"),
    ] {
        bus.publish("runtime.agent.stream",1,serde_json::json!({"lane":{"role":"writer","room":7},"event":{"type":kind,"text":text}})).unwrap();
    }
    let l = aaf_feature_context::Ledger::new(db.clone());
    l.prepare(&Lane::new("writer", Some(7)), true, &[]).unwrap();
    db.with(|c| {
        let n: i64 = c.query_row(
            "SELECT count(*) FROM context_sources WHERE body LIKE '%PARTIAL-CANARY%'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(
            n, 1,
            "partial observable response is a durable crash suffix"
        );
        let private: i64 = c.query_row(
            "SELECT count(*) FROM context_sources WHERE body LIKE '%PRIVATE-THOUGHT%'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(private, 0);
        Ok(())
    })
    .unwrap();
}

#[test]
fn first_start_does_not_backfill_history_and_catch_up_is_bounded() {
    use aaf_runtime::{continuity::Continuity, dispatcher::Lane, DriverEvent};
    let home = tempfile::tempdir().unwrap();
    let db = Db::open(&home.path().join("state.db")).unwrap();
    db.migrate("context", Context.migrations()).unwrap();
    let bus = aaf_core::events::EventBus::new(db.clone(), 16);
    let chunk = |text: &str| {
        bus.publish(
            "runtime.agent.stream",
            1,
            serde_json::json!({"lane":{"role":"writer","room":7},"event":DriverEvent::MessageChunk{text:text.into()}}),
        )
        .unwrap()
    };
    // History recorded before this lane ever started under the ledger.
    for i in 0..300 {
        chunk(&format!("OLD-HISTORY-{i}"));
    }
    let ledger = aaf_feature_context::Ledger::new(db.clone());
    let lane = Lane::new("writer", Some(7));
    ledger.started(&lane, Some("s1"), false).unwrap();
    // Committed after start but never indexed (crash between publish and observe).
    let missed = chunk("MISSED-AFTER-START");
    ledger.prepare(&lane, true, &[]).unwrap();
    let count = |sql: &str| -> i64 {
        db.with(|c| Ok(c.query_row(sql, [], |r| r.get(0))?))
            .unwrap()
    };
    assert_eq!(
        count("SELECT count(*) FROM context_sources WHERE body LIKE '%OLD-HISTORY%'"),
        0,
        "pre-start history must not be back-filled"
    );
    assert_eq!(
        count("SELECT count(*) FROM context_sources WHERE body LIKE '%MISSED-AFTER-START%'"),
        1,
        "events after start must still be caught up"
    );
    assert_eq!(
        count("SELECT event_cursor FROM context_sessions WHERE role='writer' AND room=7"),
        missed.seq as i64,
        "cursor advances to the scanned head"
    );
    // A restart keeps the cursor instead of jumping past unindexed events.
    let later = chunk("MISSED-BEFORE-RESTART");
    ledger.started(&lane, Some("s2"), false).unwrap();
    ledger.prepare(&lane, true, &[]).unwrap();
    assert_eq!(
        count("SELECT count(*) FROM context_sources WHERE body LIKE '%MISSED-BEFORE-RESTART%'"),
        1
    );
    assert_eq!(
        count("SELECT event_cursor FROM context_sessions WHERE role='writer' AND room=7"),
        later.seq as i64
    );
}
