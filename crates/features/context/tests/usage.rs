include!("continuity.rs");

use aaf_runtime::{
    continuity::Continuity, driver::DResult, AgentStatus, ContextUsage, Driver, DriverEvent,
    PermissionDecision, TurnResult,
};

fn msg(id: i64, body: &str) -> InboxMessage {
    InboxMessage {
        id,
        from: "owner".into(),
        body: body.into(),
        room: Some(7),
        thread_id: None,
    }
}

fn done(l: &aaf_feature_context::Ledger, lane: &Lane, id: i64, body: &str) {
    let p = l.prepare(lane, false, &[msg(id, body)]).unwrap();
    l.complete(
        lane,
        &p,
        &Ok(TurnResult {
            text: "OK".into(),
            stop_reason: "end_turn".into(),
        }),
    )
    .unwrap();
}

fn usage(used: u64, size: u64, agent: &str) -> ContextUsage {
    ContextUsage {
        used,
        size,
        agent: agent.into(),
    }
}

#[test]
fn reported_usage_rotates_at_per_agent_threshold_and_resets_on_new_session() {
    let c = core(Db::open_in_memory().unwrap());
    let mut l = aaf_feature_context::Ledger::configured(&c).unwrap();
    let lane = Lane::new("writer", Some(7));
    assert_eq!(l.settings.rotate_pct, None, "default is per-agent (auto)");
    l.started(&lane, Some("one"), false).unwrap();
    done(&l, &lane, 1, "first");
    assert!(!l.rotate(&lane).unwrap(), "no usage reported yet");

    l.usage(&lane, &usage(59, 100, "claude-agent-acp")).unwrap();
    assert!(!l.rotate(&lane).unwrap());
    l.usage(&lane, &usage(60, 100, "claude-agent-acp")).unwrap();
    assert!(l.rotate(&lane).unwrap(), "Claude rotates at 60%");

    l.usage(&lane, &usage(65, 100, "codex-acp")).unwrap();
    assert!(!l.rotate(&lane).unwrap(), "Codex waits for 70%");
    l.usage(&lane, &usage(70, 100, "codex-acp")).unwrap();
    assert!(l.rotate(&lane).unwrap());

    l.usage(&lane, &usage(64, 100, "pi-acp")).unwrap();
    assert!(!l.rotate(&lane).unwrap());
    l.usage(&lane, &usage(65, 100, "pi-acp")).unwrap();
    assert!(l.rotate(&lane).unwrap(), "others rotate at 65%");

    // A native load keeps the measured usage; a fresh session clears it.
    l.started(&lane, Some("one"), true).unwrap();
    assert!(l.rotate(&lane).unwrap());
    l.started(&lane, Some("two"), false).unwrap();
    assert!(!l.rotate(&lane).unwrap(), "fresh session starts unmeasured");

    l.usage(&lane, &usage(95, 100, "claude-agent-acp")).unwrap();
    l.settings.rotate_pct = Some(0);
    assert!(!l.rotate(&lane).unwrap(), "off disables usage rotation");
    l.settings.rotate_pct = Some(90);
    assert!(
        l.rotate(&lane).unwrap(),
        "fixed percent overrides the agent default"
    );
}

#[test]
fn handoff_leads_the_continuation_once_and_keeps_excerpts_at_minimum_budget() {
    let c = core(Db::open_in_memory().unwrap());
    let mut l = aaf_feature_context::Ledger::configured(&c).unwrap();
    let lane = Lane::new("writer", Some(7));
    l.started(&lane, Some("one"), false).unwrap();
    for i in 0..12 {
        done(&l, &lane, i, &format!("requirement-{i}"));
    }
    l.handoff(&lane, "HANDOFF-MARK next step: rerun bench on board 3")
        .unwrap();
    l.started(&lane, Some("two"), false).unwrap();
    let p = l.prepare(&lane, true, &[msg(99, "continue")]).unwrap();
    assert!(p.text.contains("PREVIOUS SESSION HANDOFF"), "{}", p.text);
    assert_eq!(p.text.matches("HANDOFF-MARK").count(), 1, "{}", p.text);
    assert!(
        p.text.find("HANDOFF-MARK").unwrap() < p.text.find("requirement-11").unwrap(),
        "handoff precedes excerpts"
    );

    l.handoff(&lane, &"long handoff ".repeat(500)).unwrap();
    l.settings.budget = 1024;
    let p = l.prepare(&lane, true, &[]).unwrap();
    assert!(p.text.len() <= 1024, "{}", p.text.len());
    assert!(p.text.contains("truncated"));
    assert!(
        p.text.lines().any(|x| x.starts_with('{')),
        "a long handoff must not crowd out every verbatim excerpt"
    );
}

/// Echoes prompts (minus the injected preamble) and reports a fixed context usage.
struct Full {
    tx: tokio::sync::broadcast::Sender<DriverEvent>,
    session: String,
    prompts: Arc<Mutex<Vec<String>>>,
    used: u64,
    handoff_reply: String,
}
#[async_trait]
impl Driver for Full {
    fn kind(&self) -> &'static str {
        "test"
    }
    async fn start(&self) -> DResult<()> {
        Ok(())
    }
    async fn prompt(&self, text: &str) -> DResult<TurnResult> {
        self.prompts.lock().unwrap().push(text.into());
        let reply = if text.starts_with("aaf: your context window is nearly full") {
            self.handoff_reply.clone()
        } else {
            text.into()
        };
        Ok(TurnResult {
            text: reply,
            stop_reason: "end_turn".into(),
        })
    }
    async fn cancel(&self) -> DResult<()> {
        Ok(())
    }
    async fn respond_permission(&self, _: u64, _: PermissionDecision) -> DResult<()> {
        Ok(())
    }
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }
    fn events(&self) -> tokio::sync::broadcast::Receiver<DriverEvent> {
        self.tx.subscribe()
    }
    async fn stop(&self) -> DResult<()> {
        Ok(())
    }
    fn session_id(&self) -> Option<String> {
        Some(self.session.clone())
    }
    fn context_usage(&self) -> Option<ContextUsage> {
        Some(usage(self.used, 100, "claude-agent-acp"))
    }
}

async fn run_full(handoff_reply: &str) -> (Vec<String>, Vec<String>) {
    let home = tempfile::tempdir().unwrap();
    let c = core(Db::open_in_memory().unwrap());
    let mail = Arc::new(Mail::default());
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let n = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (p2, h) = (prompts.clone(), handoff_reply.to_string());
    let d = dispatcher(&c, mail.clone(), roster(home.path())).with_factory(move |_| {
        let i = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let drv: Arc<dyn Driver> = Arc::new(Full {
            tx: tokio::sync::broadcast::channel(16).0,
            session: format!("s{i}"),
            prompts: p2.clone(),
            // first session is nearly full; the replacement is not
            used: if i == 0 { 61 } else { 10 },
            handoff_reply: h.clone(),
        });
        Some(drv)
    });
    mail.post(1, "Decision CANARY-ORCHID; build the parser");
    d.once().await.unwrap();
    mail.post(2, "continue");
    d.once().await.unwrap();
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();
    let p = prompts.lock().unwrap().clone();
    let r = mail.replies.lock().unwrap().clone();
    (p, r)
}

#[tokio::test]
async fn nearly_full_session_writes_handoff_then_continues_in_a_fresh_one() {
    let (prompts, replies) =
        run_full("HANDOFF-ORCHID: parser half done; next run the fuzz tests").await;
    assert_eq!(prompts.len(), 3, "turn, handoff, turn: {prompts:#?}");
    assert!(prompts[1].starts_with("aaf: your context window is nearly full"));
    assert_eq!(replies.len(), 2, "handoff text is never posted to the room");
    assert!(
        replies[1].contains("PREVIOUS SESSION HANDOFF"),
        "{}",
        replies[1]
    );
    assert!(replies[1].contains("HANDOFF-ORCHID"));
    assert!(
        replies[1].contains("CANARY-ORCHID"),
        "verbatim excerpts still follow"
    );
}

#[tokio::test]
async fn unusable_handoff_falls_back_to_ledger_excerpts() {
    let (prompts, replies) = run_full("API Error: 429 rate limit exceeded").await;
    assert_eq!(prompts.len(), 3, "{prompts:#?}");
    assert!(
        !replies[1].contains("PREVIOUS SESSION HANDOFF"),
        "{}",
        replies[1]
    );
    assert!(replies[1].contains("AAF CONTINUATION"));
    assert!(replies[1].contains("CANARY-ORCHID"));
}
