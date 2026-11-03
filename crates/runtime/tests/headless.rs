//! headless driver against fake CLIs (shell scripts emitting each CLI's JSONL format).
use aaf_runtime::headless::{Cli, HeadlessDriver};
use aaf_runtime::{AgentStatus, Driver, DriverEvent, DriverSpec};
use std::path::Path;

/// Writes an executable script that logs its args (one call per line) and prints `body`.
fn fake(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\necho \"$*\" >> \"{}\"\nPROMPT=$(cat)\n{body}\n",
            dir.join(format!("{name}.args")).display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    p.display().to_string()
}

fn calls(dir: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(format!("{name}.args")))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[tokio::test]
async fn claude_presets_session_then_resumes_and_reports_usage() {
    let d = tempfile::tempdir().unwrap();
    let prog = fake(
        d.path(),
        "claude",
        r#"SID=$(echo "$*" | sed -E 's/.*(--session-id|--resume) ([^ ]+).*/\2/')
echo '{"type":"system","subtype":"init","session_id":"'"$SID"'"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"ECHO: '"$PROMPT"'"}],"usage":{"input_tokens":10,"cache_read_input_tokens":50000,"cache_creation_input_tokens":0,"output_tokens":5}}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"ECHO: '"$PROMPT"'","stop_reason":"end_turn","modelUsage":{"m":{"contextWindow":200000}}}'"#,
    );
    let drv = HeadlessDriver::new(Cli::Claude, prog, d.path().into(), vec![])
        .with_model(Some("sonnet".into()));
    drv.start().await.unwrap();
    assert!(
        !drv.session_loaded(),
        "a fresh session is not a native load"
    );
    let sid = drv
        .session_id()
        .expect("claude session id is assigned up front");
    let mut rx = drv.events();
    let r = drv.prompt("hello").await.unwrap();
    assert_eq!(
        (r.text.as_str(), r.stop_reason.as_str()),
        ("ECHO: hello", "end_turn")
    );
    let mut chunks = String::new();
    while let Ok(e) = rx.try_recv() {
        if let DriverEvent::MessageChunk { text } = e {
            chunks.push_str(&text);
        }
    }
    assert_eq!(chunks, "ECHO: hello");
    let u = drv.context_usage().unwrap();
    assert_eq!(
        (u.used, u.size, u.agent.as_str()),
        (50015, 200000, "claude-headless")
    );
    drv.prompt("again").await.unwrap();
    let c = calls(d.path(), "claude");
    assert!(c[0].contains(&format!("--session-id {sid}")), "{c:?}");
    assert!(c[1].contains(&format!("--resume {sid}")), "{c:?}");
    assert!(c[0].contains("--model sonnet") && c[0].contains("--output-format stream-json"));
    assert_eq!(drv.session_id().as_deref(), Some(sid.as_str()));
    assert_eq!(drv.status(), AgentStatus::Idle);

    // A restart with the saved id resumes natively.
    let again = HeadlessDriver::new(
        Cli::Claude,
        calls_prog(d.path(), "claude"),
        d.path().into(),
        vec![],
    )
    .with_resume(Some(sid.clone()));
    again.start().await.unwrap();
    assert!(again.session_loaded());
    again.prompt("after restart").await.unwrap();
    assert!(calls(d.path(), "claude")[2].contains(&format!("--resume {sid}")));
}

#[tokio::test]
async fn claude_rate_limit_event_becomes_quota_windows() {
    let d = tempfile::tempdir().unwrap();
    let prog = fake(
        d.path(),
        "claude",
        r#"echo '{"type":"system","subtype":"init","session_id":"s"}'
echo '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.25,"resetsAt":1793354400},"seven_day":{"utilization":0.5,"resetsAt":1793577600}}}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"ok","stop_reason":"end_turn"}'"#,
    );
    let drv = HeadlessDriver::new(Cli::Claude, prog, d.path().into(), vec![])
        .with_account(Some("work".into()));
    drv.start().await.unwrap();
    assert!(drv.rate_limit().is_none(), "nothing before the first turn");
    drv.prompt("hi").await.unwrap();
    let v = drv.rate_limit().expect("windows after a turn");
    assert_eq!(v["account"], "work");
    assert_eq!(v["windows"][0]["label"], "5 hours");
    assert_eq!(v["windows"][0]["remaining_percent"], 75.0);
    assert_eq!(v["windows"][1]["remaining_percent"], 50.0);
}

fn calls_prog(dir: &Path, name: &str) -> String {
    dir.join(name).display().to_string()
}

#[tokio::test]
async fn codex_learns_thread_id_and_resumes_it() {
    let d = tempfile::tempdir().unwrap();
    let prog = fake(
        d.path(),
        "codex",
        r#"echo '{"type":"thread.started","thread_id":"th-1"}'
echo '{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"aaf task list"}}'
echo '{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"aaf task list"}}'
echo '{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"done: '"$PROMPT"'"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":3000,"output_tokens":7}}'"#,
    );
    let drv = HeadlessDriver::new(Cli::Codex, prog, d.path().into(), vec![]);
    drv.start().await.unwrap();
    assert_eq!(
        drv.session_id(),
        None,
        "codex reports its thread id on the first turn"
    );
    let mut rx = drv.events();
    let r = drv.prompt("x").await.unwrap();
    assert_eq!(r.text, "done: x");
    let tools = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|e| matches!(e, DriverEvent::ToolCall { title, .. } if title == "aaf task list"))
        .count();
    assert_eq!(tools, 2, "start and completion of the command");
    assert_eq!(drv.session_id().as_deref(), Some("th-1"));
    drv.prompt("y").await.unwrap();
    let c = calls(d.path(), "codex");
    assert!(!c[0].contains("resume") && c[0].ends_with(" -"), "{c:?}");
    assert!(c[1].ends_with("resume th-1 -"), "{c:?}");
    assert!(
        c[0].contains("--sandbox workspace-write"),
        "default permissions: {c:?}"
    );
}

#[tokio::test]
async fn pi_errors_and_failed_exits_become_error_turns() {
    let d = tempfile::tempdir().unwrap();
    let prog = fake(
        d.path(),
        "pi",
        r#"echo '{"type":"session","id":"p-1"}'
echo '{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"429 rate limited"}}'
echo '{"type":"agent_settled","aborted":false}'"#,
    );
    let drv = HeadlessDriver::new(Cli::Pi, prog, d.path().into(), vec![]);
    drv.start().await.unwrap();
    let r = drv.prompt("q").await.unwrap();
    assert_eq!(r.stop_reason, "error: 429 rate limited");
    assert!(calls(d.path(), "pi")[0].contains("--mode json"));

    let crash = fake(d.path(), "pi2", "echo boom >&2\nexit 3");
    let drv = HeadlessDriver::new(Cli::Pi, crash, d.path().into(), vec![]);
    drv.start().await.unwrap();
    let r = drv.prompt("q").await.unwrap();
    assert!(
        r.stop_reason.starts_with("error: pi exited"),
        "{}",
        r.stop_reason
    );
    assert!(r.stop_reason.contains("boom"));
}

#[tokio::test]
async fn cancel_kills_a_running_turn() {
    let d = tempfile::tempdir().unwrap();
    let prog = fake(d.path(), "claude", "sleep 30");
    let drv = std::sync::Arc::new(HeadlessDriver::new(
        Cli::Claude,
        prog,
        d.path().into(),
        vec![],
    ));
    drv.start().await.unwrap();
    let run = {
        let drv = drv.clone();
        tokio::spawn(async move { drv.prompt("long").await })
    };
    for _ in 0..50 {
        if drv.status() == AgentStatus::Working {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let t0 = std::time::Instant::now();
    drv.cancel().await.unwrap();
    let r = run.await.unwrap().unwrap();
    assert_eq!(r.stop_reason, "cancelled");
    assert!(t0.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn spec_selects_headless_only_for_supported_agents() {
    let spec = |agent: &str, transport: Option<&str>| {
        serde_json::from_value::<DriverSpec>(serde_json::json!({
            "driver": "acp", "acp_agent": agent, "transport": transport
        }))
        .unwrap()
    };
    assert_eq!(
        spec("claude", Some("headless")).headless(),
        Some(Cli::Claude)
    );
    assert_eq!(spec("pi", Some("headless")).headless(), Some(Cli::Pi));
    assert_eq!(
        spec("agy", Some("headless")).headless(),
        None,
        "agy has no headless CLI"
    );
    assert_eq!(spec("codex", None).headless(), None);
    let d = tempfile::tempdir().unwrap();
    let built = spec("codex", Some("headless"))
        .build_resuming(d.path(), None, &Default::default())
        .unwrap();
    assert_eq!(built.kind(), "headless");
}
