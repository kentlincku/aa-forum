//! 程序生命週期與 env 樣板：真實子程序、不用 doubles。

use aaf_runtime::acp::AcpDriver;
use aaf_runtime::command::CommandDriver;
use aaf_runtime::driver::{expand_template, resolve_env, LaunchContext, PROTECTED_ENV};
use aaf_runtime::process::{reap, reap_orphans, start_time, AgentRecord, ReapOutcome, Registry};
use aaf_runtime::{Driver, DriverSpec};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn alive(pid: u32) -> bool {
    start_time(pid).is_some()
}

/// 寫一支 shell agent：在背景開一個孫程序（sleep），把孫程序 pid 寫到檔案，然後等待。
fn spawner(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    let script = dir.join("spawn.sh");
    let out = dir.join("grandchild.pid");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 300 &\necho $! > {}\nwait\n",
            out.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (script, out)
}

async fn read_pid(p: &std::path::Path) -> u32 {
    for _ in 0..200 {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Ok(n) = s.trim().parse() {
                return n;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("pid file not written");
}

#[tokio::test]
async fn acp_stop_kills_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let (script, out) = spawner(dir.path());
    let reg = Registry::new(dir.path()).unwrap();
    // 不是 ACP agent：initialize 會逾時，但程序已啟動；用短逾時
    let mut d = AcpDriver::new(
        vec![script.to_string_lossy().into()],
        dir.path().into(),
        vec![],
    )
    .with_registry(Some(reg.clone()));
    d.request_timeout = Duration::from_millis(300);
    let d = Arc::new(d);
    let d2 = d.clone();
    let start = tokio::spawn(async move { d2.start().await });
    let grandchild = read_pid(&out).await;
    assert!(alive(grandchild));
    assert_eq!(reg.records().len(), 1, "agent recorded at spawn");
    assert!(start.await.unwrap().is_err(), "not an ACP agent");
    // start 失敗時 conn 未建立；用紀錄確認 stop/reap 可清掉整組
    let rec = reg.records().pop().unwrap();
    assert_eq!(
        reap(&rec, Duration::from_secs(2)).await,
        ReapOutcome::Stopped
    );
    for _ in 0..100 {
        if !alive(grandchild) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(grandchild), "grandchild killed with the group");
}

#[tokio::test]
async fn command_cancel_kills_grandchildren() {
    let dir = tempfile::tempdir().unwrap();
    let (script, out) = spawner(dir.path());
    let reg = Registry::new(dir.path()).unwrap();
    let d = Arc::new(
        CommandDriver::new(vec![script.to_string_lossy().into()], dir.path().into())
            .with_registry(Some(reg.clone())),
    );
    let d2 = d.clone();
    let turn = tokio::spawn(async move { d2.prompt("x").await });
    let grandchild = read_pid(&out).await;
    assert_eq!(reg.records().len(), 1);
    d.cancel().await.unwrap();
    assert_eq!(turn.await.unwrap().unwrap().stop_reason, "cancelled");
    for _ in 0..100 {
        if !alive(grandchild) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(grandchild), "grandchild killed on cancel");
    assert!(reg.records().is_empty(), "record removed after turn");
}

#[tokio::test]
async fn reap_orphans_only_kills_matching_identity() {
    let dir = tempfile::tempdir().unwrap();
    let reg = Registry::new(dir.path()).unwrap();
    // 真正的孤兒：獨立程序群組的 sleep
    let mut c = std::process::Command::new("/bin/sleep");
    c.arg("300");
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    let mut orphan = c.spawn().unwrap();
    let pid = orphan.id();
    reg.record(pid, "sleep 300").unwrap();

    // 身分不符（模擬 pid 重用）：另一個存活程序，但紀錄的出生時間是錯的
    let mut other = std::process::Command::new("/bin/sleep")
        .arg("300")
        .spawn()
        .unwrap();
    let wrong = AgentRecord {
        pid: other.id(),
        started: start_time(other.id()).unwrap() - 1000,
        command: "reused".into(),
    };
    std::fs::write(
        dir.path()
            .join("agents")
            .join(format!("{}.json", other.id())),
        serde_json::to_vec(&wrong).unwrap(),
    )
    .unwrap();

    // 已不存在的程序
    let gone = AgentRecord {
        pid: 999_999,
        started: 1,
        command: "gone".into(),
    };
    std::fs::write(
        dir.path().join("agents").join("999999.json"),
        serde_json::to_vec(&gone).unwrap(),
    )
    .unwrap();

    let mut res = reap_orphans(&reg, Duration::from_secs(2)).await;
    res.sort_by_key(|(r, _)| r.pid);
    let outcomes: Vec<_> = res.iter().map(|(r, o)| (r.command.as_str(), *o)).collect();
    assert!(outcomes.contains(&("sleep 300", ReapOutcome::Stopped)));
    assert!(outcomes.contains(&("reused", ReapOutcome::Unverified)));
    assert!(outcomes.contains(&("gone", ReapOutcome::Absent)));

    let _ = orphan.wait();
    assert!(
        alive(other.id()),
        "process with mismatched identity must not be killed"
    );
    assert!(reg.records().is_empty(), "all records resolved");
    let _ = other.kill();
    let _ = other.wait();
}

#[test]
fn env_template_expands_once_and_blocks_identity_vars() {
    let ctx = LaunchContext {
        role: "coder".into(),
        instance: PathBuf::from("/srv/{role}-inst"),
        agents: None,
        extra_env: vec![],
    };
    assert_eq!(
        expand_template("{instance}/state/{role}", &ctx),
        "/srv/{role}-inst/state/coder",
        "inserted braces not re-expanded"
    );
    assert_eq!(expand_template("{unknown} {", &ctx), "{unknown} {");
    let env = resolve_env(&[("XDG_STATE_HOME".into(), "{instance}/s".into())], &ctx).unwrap();
    assert!(env.contains(&("XDG_STATE_HOME".into(), "/srv/{role}-inst/s".into())));
    assert!(env.contains(&("AAF_ROLE".into(), "coder".into())));
    for k in PROTECTED_ENV {
        assert!(
            resolve_env(&[(k.to_string(), "x".into())], &ctx).is_err(),
            "{k} must be protected"
        );
    }
}

#[tokio::test]
async fn role_env_reaches_agent_and_roles_json_rejects_protected() {
    let dir = tempfile::tempdir().unwrap();
    let json = r#"{"roles": {"probe": {"driver": "acp", "command": ["x"], "env": {"MBOX_TOKEN": "steal"}}}}"#;
    assert!(aaf_runtime::dispatcher::Roster::from_json(json, dir.path()).is_err());
    let ok = r#"{"roles": {"probe": {"driver": "acp", "command": ["x"], "env": {"STATE": "{instance}/st/{role}"}}}}"#;
    let r = aaf_runtime::dispatcher::Roster::from_json(ok, dir.path()).unwrap();
    assert!(
        matches!(&r.roles["probe"].driver, DriverSpec::Acp { env, .. } if env == &vec![("STATE".to_string(), "{instance}/st/{role}".to_string())])
    );

    // command driver 收到 AAF_ROLE 與 AAF_HOME
    let ctx = LaunchContext {
        role: "probe".into(),
        instance: dir.path().into(),
        agents: None,
        extra_env: vec![],
    };
    let spec = DriverSpec::Command {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo $AAF_ROLE:$AAF_HOME".into(),
        ],
        cwd: None,
    };
    let d = spec.build_resuming(dir.path(), None, &ctx).unwrap();
    assert_eq!(
        d.prompt("").await.unwrap().text,
        format!("probe:{}", dir.path().display())
    );
}
