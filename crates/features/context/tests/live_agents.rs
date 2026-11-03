// Live check against a real agent (spends tokens). Not part of CI:
//   LIVE_CMD=/path/to/codex-acp cargo test -p aaf-feature-context --test live_agents -- --ignored --nocapture
//   LIVE_HEADLESS=claude|codex|pi [LIVE_MODEL=...] cargo test ... (same, runs the agent's own CLI)
include!("continuity.rs");

use aaf_runtime::headless::{Cli, HeadlessDriver};
use aaf_runtime::{acp::AcpDriver, Driver};

fn live_driver(cwd: &std::path::Path, resume: Option<String>) -> Arc<dyn Driver> {
    match std::env::var("LIVE_HEADLESS") {
        Ok(a) => {
            let cli = Cli::from_agent(&a).expect("LIVE_HEADLESS=claude|codex|pi");
            let program = aaf_runtime::catalog::which(cli.program())
                .expect("CLI on PATH")
                .display()
                .to_string();
            Arc::new(
                HeadlessDriver::new(cli, program, cwd.into(), vec![])
                    .with_model(std::env::var("LIVE_MODEL").ok())
                    .with_resume(resume),
            )
        }
        Err(_) => Arc::new(
            AcpDriver::new(
                vec![std::env::var("LIVE_CMD").expect("LIVE_CMD")],
                cwd.into(),
                vec![],
            )
            .with_resume(resume),
        ),
    }
}

/// Headless only: a new driver resumed with the saved id still has the conversation (no aaf injection).
#[tokio::test]
#[ignore]
async fn live_headless_native_resume() {
    if std::env::var("LIVE_HEADLESS").is_err() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(work.path()).unwrap();
    let a = live_driver(&cwd, None);
    a.start().await.unwrap();
    let r = a
        .prompt("Remember the code word AMBER-FINCH. Do not use tools. Reply with only OK.")
        .await
        .unwrap();
    println!("RESUME turn 1: {r:?} usage={:?}", a.context_usage());
    let sid = a.session_id().expect("session id after a turn");
    let b = live_driver(&cwd, Some(sid.clone()));
    b.start().await.unwrap();
    assert!(b.session_loaded());
    let r = b
        .prompt("What was the code word? Do not use tools. Reply with only the code word.")
        .await
        .unwrap();
    println!("RESUME turn 2 ({sid}): {r:?}");
    assert!(r.text.contains("AMBER-FINCH"), "{r:?}");
}

/// (ctx_used, ctx_size, ctx_agent, session)
type UsageRow = (Option<i64>, Option<i64>, Option<String>, String);

#[tokio::test]
#[ignore]
async fn live_connect_usage_and_handoff() {
    let work = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(work.path()).unwrap();

    if let Ok(cmd) = std::env::var("LIVE_CMD") {
        let t0 = std::time::Instant::now();
        let check = aaf_runtime::catalog::check_connect(
            std::slice::from_ref(&cmd),
            &[],
            &cwd,
            "",
            "",
            std::time::Duration::from_secs(90),
        )
        .await;
        println!("CONNECT ({:.1}s): {check}", t0.elapsed().as_secs_f64());
    }

    let c = core(Db::open_in_memory().unwrap());
    let mut ledger = aaf_feature_context::Ledger::configured(&c).unwrap();
    ledger.settings.rotate_pct = Some(1); // force rotation after the first turn
    let mail = Arc::new(Mail::default());
    let cwd2 = cwd.clone();
    let d = Dispatcher::new(mail.clone(), c.bus.clone(), roster(&cwd))
        .with_continuity(Some(Arc::new(ledger)))
        .with_factory(move |_| Some(live_driver(&cwd2, None)));

    mail.post(
        1,
        "Remember this code word: TEAL-ORCHID. Do not use tools. Reply with one short sentence.",
    );
    d.once().await.unwrap();
    let usage: Vec<UsageRow> =
        c.db.with(|c| {
            let mut st =
                c.prepare("SELECT ctx_used,ctx_size,ctx_agent,session FROM context_sessions")?;
            let r = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(r)
        })
        .unwrap();
    println!("USAGE after turn 1: {usage:?}");

    mail.post(
        2,
        "What was the code word? Do not use tools. Reply with only the code word.",
    );
    d.once().await.unwrap();
    d.close(&Lane::new("writer", Some(7)), false).await.unwrap();

    let (events, handoffs, sessions): (Vec<String>, Vec<String>, Vec<String>) =
        c.db.with(|c| {
            let q = |sql: &str| -> rusqlite::Result<Vec<String>> {
                let mut st = c.prepare(sql)?;
                let r = st.query_map([], |r| r.get(0))?.collect();
                r
            };
            Ok((
                q("SELECT payload FROM events WHERE kind='runtime.context.handoff'")?,
                q("SELECT body FROM context_sources WHERE kind='handoff'")?,
                q("SELECT session FROM context_sessions")?,
            ))
        })
        .unwrap();
    println!("HANDOFF EVENTS: {events:#?}");
    println!("HANDOFF SOURCES: {handoffs:#?}");
    println!("SESSION NOW: {sessions:?}");
    for (i, r) in mail.replies.lock().unwrap().iter().enumerate() {
        println!(
            "REPLY {}: {}",
            i + 1,
            r.chars().take(400).collect::<String>()
        );
    }
}
