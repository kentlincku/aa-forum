//! Native owned-child contract: no mocks of process state or reaping.
#![cfg(unix)]
use aaf_runtime::process::{isolate, ReapOutcome};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires AAF_REVIEW_DENY_GROUP=1 and DYLD fault injector"]
async fn native_eperm_retains_connection_registry_and_unreaped_leader() {
    use aaf_runtime::process::{start_time, Registry};
    use aaf_runtime::{acp::AcpDriver, Driver};
    assert_eq!(std::env::var("AAF_REVIEW_DENY_GROUP").unwrap(), "1");
    let home = tempfile::tempdir().unwrap();
    let reg = Registry::new(home.path()).unwrap();
    let driver = AcpDriver::new(
        vec![
            "/usr/bin/python3".into(),
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/owned_tree.py").into(),
            home.path().display().to_string(),
        ],
        home.path().into(),
        vec![],
    )
    .with_registry(Some(reg.clone()));
    driver.start().await.unwrap();
    let record = reg.records().pop().unwrap();
    let descendant: u32 = std::fs::read_to_string(home.path().join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let birth = start_time(descendant).unwrap();
    let mut control = tokio::process::Command::new("/bin/sleep");
    control.arg("15").kill_on_drop(true);
    isolate(&mut control);
    let mut control = control.spawn().unwrap();
    let control_pid = control.id().unwrap();
    let control_birth = start_time(control_pid).unwrap();
    let started = tokio::time::Instant::now();
    for _ in 0..2 {
        assert!(
            driver.stop().await.is_err(),
            "a second refusal proves conn was retained"
        );
        assert_eq!(reg.records(), vec![record.clone()]);
        assert_eq!(start_time(descendant), Some(birth));
        assert_eq!(start_time(control_pid), Some(control_birth));
    }
    assert!(started.elapsed() < Duration::from_secs(3));
    std::fs::write(home.path().join("release"), "cooperative owned cleanup").unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(start_time(descendant), None);
    // Darwin keeps the leader waitable, though start_time may omit its zombie.
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &record.pid.to_string()])
        .output()
        .unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .starts_with('Z'),
        "leader was not reaped while ownership was unresolved"
    );
    driver.stop().await.unwrap();
    assert!(reg.records().is_empty());
    assert_eq!(start_time(control_pid), Some(control_birth));
    control.kill().await.unwrap();
    control.wait().await.unwrap();
}

async fn child(script: &str) -> tokio::process::Child {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.args(["-c", script]).kill_on_drop(false);
    isolate(&mut cmd);
    cmd.spawn().unwrap()
}

#[tokio::test]
async fn stopped_group_preserves_unrelated_group_and_reaps_once() {
    let mut owned = child("sleep 20 & wait").await;
    let mut control = child("sleep 20 & wait").await;
    let control_pid = control.id().unwrap();
    let birth = aaf_runtime::process::start_time(control_pid).unwrap();
    assert_eq!(
        aaf_runtime::process::stop_owned(&mut owned, Duration::from_millis(80)).await,
        ReapOutcome::Stopped
    );
    assert!(
        owned.id().is_none(),
        "reap only after the entire group is quiescent"
    );
    assert_eq!(aaf_runtime::process::start_time(control_pid), Some(birth));
    assert_eq!(
        aaf_runtime::process::stop_owned(&mut owned, Duration::from_millis(80)).await,
        ReapOutcome::Absent
    );
    assert_eq!(
        aaf_runtime::process::stop_owned(&mut control, Duration::from_millis(80)).await,
        ReapOutcome::Stopped
    );
}

#[tokio::test]
async fn unreaped_zombie_group_can_be_stopped_without_signalling_reused_pid() {
    let mut owned = child("exit 0").await;
    let pid = owned.id().unwrap();
    // A native process-table observation preserves the process-group reservation.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let out = std::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        if String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .starts_with('Z')
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owned child did not become a zombie"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    #[cfg(target_os = "macos")]
    assert_eq!(
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM
        ),
        Err(nix::errno::Errno::EPERM),
        "native Darwin zombie signal is not proof of a live group"
    );
    assert_eq!(
        aaf_runtime::process::stop_owned(&mut owned, Duration::from_millis(80)).await,
        ReapOutcome::Stopped
    );
    assert!(owned.id().is_none());
}

#[tokio::test]
async fn already_reaped_native_child_is_not_signalled() {
    let mut owned = child("exit 0").await;
    let pid = owned.id().unwrap();
    nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid as i32), None).unwrap();
    // Even if a foreign reaper violated Child's ownership, refuse all signals.
    assert_eq!(
        aaf_runtime::process::stop_owned(&mut owned, Duration::from_millis(80)).await,
        ReapOutcome::Unverified
    );
}
