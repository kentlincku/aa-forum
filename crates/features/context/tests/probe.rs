// Adapter: relative includes only; reviewer inputs/assertions unchanged.
// Read-only source inclusion: all binaries/build output belong to scratch.
include!("continuity.rs");
#[cfg(test)]
mod ledger_suite {
    include!("ledger.rs");
}
#[cfg(test)]
mod http_suite {
    include!("http.rs");
    #[tokio::test]
    async fn probe_enumerate_every_source_and_retrieve_full_tool_json() {
        let mut reg = Registry::default();
        reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
        reg.register(Box::new(Context)).unwrap();
        let core = Arc::new(
            Core::prepare(
                &reg,
                Db::open_in_memory().unwrap(),
                BootOptions {
                    env: vec![("AAF_CONTEXT_PAGE_BYTES".into(), "37".into())],
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let tool=json!({"id":"tool-old","input":{"cmd":"untrusted command"},"output":"歷史工具結果𠮷".repeat(200),"locations":["artifact.txt"],"coverage":"effects unverified"}).to_string();
        core.db.with(|c| {
            c.execute("INSERT INTO context_sources(role,room,kind,body) VALUES('writer',7,'tool',?1)",[&tool])?;
            for i in 0..702 {c.execute("INSERT INTO context_sources(role,room,kind,body) VALUES('writer',7,'input',?1)",[json!({"body":format!("requirement-{i}")}).to_string()])?;}
            c.execute("INSERT INTO context_sources(role,room,kind,body) VALUES('writer',8,'input','{\"secret\":true}')",[])?;
            Ok(())
        }).unwrap();
        let token = aaf_api::issue_role_token(&core, "writer", "worker").unwrap();
        let http = aaf_feature_context::http::ContextHttp::new(Arc::new(|_, _, _| Ok(true)));
        let app = aaf_api::app(core, &[Arc::new(http)]);
        let root = "/api/v1/context/rooms/7/roles/writer/sources";
        let mut after = 0;
        let mut ids = std::collections::BTreeSet::new();
        let mut pages = 0;
        loop {
            let (s, v) = get(&app, &token, &format!("{root}?after={after}&limit=17")).await;
            assert_eq!(s, 200);
            pages += 1;
            for row in v["sources"].as_array().unwrap() {
                assert!(
                    row["kind"].is_string() && row["bytes"].is_i64() && row["excerpt"].is_string()
                );
                assert!(ids.insert(row["id"].as_i64().unwrap()));
            }
            let Some(next) = v["next_after"].as_i64() else {
                break;
            };
            assert!(next > after);
            after = next;
        }
        assert_eq!(ids.len(), 703);
        assert!(!ids.contains(&704));
        let mut offset = 0;
        let mut full = String::new();
        loop {
            let (s, v) = get(&app, &token, &format!("{root}?source=1&offset={offset}")).await;
            assert_eq!(s, 200);
            assert_eq!(v["offset"], offset);
            assert_eq!(v["total_bytes"], tool.len());
            let part = v["text"].as_str().unwrap();
            assert!(part.len() <= 37);
            full.push_str(part);
            let Some(next) = v["next_offset"].as_u64() else {
                break;
            };
            assert_eq!(next as usize, full.len());
            offset = next;
        }
        assert_eq!(full, tool);
        println!(
            "PAGED_RETRIEVAL source_count={} index_pages={pages} full_tool_bytes={}",
            ids.len(),
            full.len()
        );
    }
}
#[cfg(test)]
mod chat_suite {
    include!("chat.rs");
}

#[tokio::test]
async fn probe_ordinary_checkpoint_failure_requires_recovery_package() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let r = acp_roster(home.path(), false);
    let d = dispatcher(&c, mail.clone(), r.clone());
    let lane = Lane::new("writer", Some(7));
    mail.post(1, "EARLY-REQUIREMENT no side-effect replay");
    d.once().await.unwrap();
    // Unlike the supplied regression, do NOT create a new session before failure.
    c.db.with(|c| { c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints BEGIN SELECT RAISE(ABORT,'ordinary checkpoint failure'); END;")?; Ok(()) }).unwrap();
    mail.post(2, "ordinary turn can have unknown external effects");
    assert!(d.once().await.is_err());
    assert_eq!(mail.queued.lock().unwrap().len(), 1);
    let pending: bool =
        c.db.with(|c| Ok(c.query_row("SELECT pending FROM context_sessions", [], |r| r.get(0))?))
            .unwrap();
    d.close(&lane, false).await.unwrap();
    c.db.with(|c| {
        c.execute_batch("DROP TRIGGER fail_checkpoint;")?;
        Ok(())
    })
    .unwrap();
    drop(d);
    let d = dispatcher(&c, mail.clone(), r);
    d.once().await.unwrap();
    let reply = mail.replies.lock().unwrap().last().unwrap().clone();
    d.close(&lane, false).await.unwrap();
    println!(
        "ORDINARY_FAILURE pending={pending}; recovery_injected={}; reply={reply:?}",
        reply.contains("AAF CONTINUATION")
    );
    assert!(
        reply.contains("AAF CONTINUATION"),
        "ordinary checkpoint error + native load loses recovery marker"
    );
}

#[tokio::test]
async fn probe_idle_parking_must_not_clear_context_fault_latch() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let mut d = dispatcher(&c, mail.clone(), acp_roster(home.path(), false));
    d.idle_park = std::time::Duration::ZERO;
    mail.post(1, "effect must not repeat automatically");
    c.db.with(|c| {c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON context_checkpoints BEGIN SELECT RAISE(ABORT,'checkpoint failure'); END;")?;Ok(())}).unwrap();
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
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
    println!("FAULT_LATCH parked={parked:?} output_attempts_before={before} after={after}");
    assert_eq!(
        before, after,
        "automatic idle parking must not retry unknown external effects"
    );
}

struct StopFails {
    inner: FinalEvidence,
}
#[async_trait]
impl aaf_runtime::Driver for StopFails {
    fn kind(&self) -> &'static str {
        "stop-failure-probe"
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
        aaf_runtime::Driver::events(&self.inner)
    }
    async fn stop(&self) -> aaf_runtime::driver::DResult<()> {
        Err(aaf_runtime::driver::DriverError::Protocol(
            "executor refused stop; still alive".into(),
        ))
    }
}
#[tokio::test]
async fn probe_switch_must_fail_closed_when_stop_fails() {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let driver = Arc::new(StopFails {
        inner: FinalEvidence {
            tx: tokio::sync::broadcast::channel(16).0,
            status: Mutex::new(aaf_runtime::AgentStatus::Offline),
        },
    });
    let old = driver.clone();
    let d = dispatcher(&c, mail.clone(), roster(home.path()))
        .with_factory(move |_| Some(driver.clone()));
    mail.post(1, "first executor");
    d.once().await.unwrap();
    let mut changed = roster(home.path());
    changed.roles.get_mut("writer").unwrap().driver = DriverSpec::Command {
        command: vec!["/bin/sh".into(), "-c".into(), "cat".into()],
        cwd: None,
    };
    let switched = d.apply_roster(changed).await;
    println!(
        "STOP_FAILURE switch_ok={} old_status={:?} registered_old={}",
        switched.is_ok(),
        aaf_runtime::Driver::status(old.as_ref()),
        d.lane_driver(&Lane::new("writer", Some(7))).await.is_some()
    );
    assert!(
        switched.is_err(),
        "cannot acknowledge successful replacement when old executor stop failed"
    );
}

#[test]
#[ignore = "Historical observation assumes repeated rotation; retained unchanged, replaced by normative accounting test"]
fn probe_rotation_accounting() {
    use aaf_runtime::{continuity::Continuity, DriverEvent, TurnResult};
    let c = core(Db::open_in_memory().unwrap());
    let mut l = aaf_feature_context::Ledger::configured(&c).unwrap();
    let lane = Lane::new("writer", Some(7));
    // A small but accepted setting can be exceeded by the continuation alone.
    l.settings.rotate_bytes = 64;
    l.started(&lane, Some("one"), false).unwrap();
    let p = l
        .prepare(
            &lane,
            false,
            &[InboxMessage {
                id: 1,
                from: "owner".into(),
                body: "initial requirement".repeat(10),
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
    for n in 2..=4 {
        assert!(l.rotate(&lane).unwrap());
        l.started(&lane, Some(&format!("s{n}")), false).unwrap();
        let p = l
            .prepare(
                &lane,
                true,
                &[InboxMessage {
                    id: n,
                    from: "owner".into(),
                    body: "ok".into(),
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
        let bytes: i64 =
            c.db.with(|c| Ok(c.query_row("SELECT bytes FROM context_sessions", [], |r| r.get(0))?))
                .unwrap();
        println!(
            "LOW_THRESHOLD turn={n} package_bytes={} observed_counter={bytes} rotate_again={}",
            p.text.len(),
            l.rotate(&lane).unwrap()
        );
    }
    // Tool results consume model context but are not counted at all.
    l.settings.rotate_bytes = 262144;
    l.started(&lane, Some("tool-heavy"), false).unwrap();
    l.observed(
        &lane,
        999,
        &DriverEvent::ToolEvidence {
            data: serde_json::json!({"id":"large","output":"x".repeat(300000)}),
        },
    )
    .unwrap();
    println!(
        "TOOL_HEAVY output_bytes=300000 rotate={}",
        l.rotate(&lane).unwrap()
    );
    // JSON escaping can cross default threshold before raw text does.
    let p = aaf_runtime::continuity::PreparedContext::default();
    l.complete(
        &lane,
        &p,
        &Ok(TurnResult {
            text: "\n".repeat(131100),
            stop_reason: "end_turn".into(),
        }),
    )
    .unwrap();
    let bytes: i64 =
        c.db.with(|c| Ok(c.query_row("SELECT bytes FROM context_sessions", [], |r| r.get(0))?))
            .unwrap();
    println!(
        "ESCAPED_OUTPUT raw_bytes=131100 counter={bytes} rotate={}",
        l.rotate(&lane).unwrap()
    );
}

#[test]
fn probe_minimum_budget_source_index_disappears() {
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
    println!(
        "MINIMUM_BUDGET bytes={} indexed_sources={} text={:?}",
        p.text.len(),
        p.text.lines().filter(|s| s.starts_with('{')).count(),
        p.text
    );
}
