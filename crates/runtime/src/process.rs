//! agent 子程序的生命週期：獨立程序群組、整組終止、啟動時清理上次殘留。
//!
//! - 每個 agent 以自己的程序群組啟動（setsid 等價：process_group(0)），停止時對整組送訊號，
//!   agent 再開的子程序（例如 codex-acp 啟動的 codex）一併結束。
//! - 啟動時把「pid＋出生時間」記到 `<state>/agents/<pid>.json`；正常結束即刪除。
//! - aaf 被 SIGKILL 時這些紀錄會留下；下次啟動 [`reap_orphans`] 只在 pid 的出生時間與紀錄
//!   完全相符時才終止該程序群組。對不上（pid 已被重用）或查不到，一律不動、只移除紀錄（fail closed）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub pid: u32,
    /// 程序出生時間（秒，sysinfo start_time）；與 pid 一起構成身分。
    pub started: u64,
    pub command: String,
}

/// 結果分三種：確定停止、本來就不在、無法確認（不動作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapOutcome {
    Stopped,
    Absent,
    Unverified,
}

/// 取程序出生時間；查不到回 None。
pub fn start_time(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(p).map(|x| x.start_time())
}

/// 在指令上設定獨立程序群組（Unix）。
pub fn isolate(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Legacy command-driver entry point. Its caller still owns an unreaped child.
/// Keep this compatibility API noisy on failure; ACP uses `stop_owned` and retains ownership.
pub async fn kill_group(pgid: u32, grace: Duration) {
    #[cfg(unix)]
    let outcome = stop_group(pgid, grace, || owned_unreaped(pgid)).await;
    #[cfg(not(unix))]
    let outcome = {
        let _ = (pgid, grace);
        ReapOutcome::Unverified
    };
    if outcome == ReapOutcome::Unverified {
        tracing::error!(
            pgid,
            "command process-group stop unverified; cleanup not confirmed"
        );
    }
}

/// OS process-table census, not signal permission as a proxy for death.
/// Zombie-only groups are quiescent on Darwin even when killpg returns EPERM.
#[cfg(unix)]
async fn group_snapshot(pgid: u32) -> Option<(bool, bool)> {
    let mut cmd = tokio::process::Command::new("/bin/ps");
    cmd.args(["-axo", "pid=,ppid=,pgid=,stat="])
        .env("LC_ALL", "C")
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_millis(500), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&out.stdout).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    let mut quiet = true;
    let mut owned = false;
    for row in text.lines() {
        let mut fields = row.split_whitespace();
        let pid: u32 = fields.next()?.parse().ok()?;
        let parent: u32 = fields.next()?.parse().ok()?;
        let group: u32 = fields.next()?.parse().ok()?;
        let status = fields.next()?;
        if group == pgid && !status.starts_with('Z') {
            quiet = false;
        }
        if pid == pgid && group == pgid && parent == std::process::id() {
            owned = true;
        }
    }
    Some((quiet, owned))
}

#[cfg(unix)]
async fn group_quiet(pgid: u32) -> Option<bool> {
    group_snapshot(pgid).await.map(|s| s.0)
}

#[cfg(unix)]
async fn owned_unreaped(pid: u32) -> bool {
    // Child has exactly one reaper under the owner lock. A native process-table
    // row (including Darwin zombies omitted by proc_pidinfo) confirms the reservation
    // without polling Child, which would reap. Missing/inaccessible rows fail closed.
    group_snapshot(pid).await.is_some_and(|s| s.1)
}

#[cfg(unix)]
async fn stop_group<F, Fut>(pgid: u32, grace: Duration, owns: F) -> ReapOutcome
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    if pgid == 0 || pgid > i32::MAX as u32 || !owns().await {
        return ReapOutcome::Unverified;
    }
    let g = Pid::from_raw(pgid as i32);
    // The ownership predicate is rechecked before every destructive signal.
    // No live member can fork once the census reports a zombie-only/empty group.
    let first = killpg(g, Signal::SIGTERM);
    if first.is_err() {
        return if group_quiet(pgid).await == Some(true) {
            ReapOutcome::Stopped
        } else {
            ReapOutcome::Unverified
        };
    }
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        match group_quiet(pgid).await {
            Some(true) => return ReapOutcome::Stopped,
            None => return ReapOutcome::Unverified,
            Some(false) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if !owns().await {
        return ReapOutcome::Unverified;
    }
    let killed = killpg(g, Signal::SIGKILL);
    // SIGKILL success is only submission, not evidence that the group is stopped.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        match group_quiet(pgid).await {
            Some(true) => return ReapOutcome::Stopped,
            None => return ReapOutcome::Unverified,
            Some(false) => {}
        }
        if killed.is_err() || tokio::time::Instant::now() >= deadline {
            return ReapOutcome::Unverified;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sole-reaper operation. Caller holds its child/owner lock throughout; failure keeps the
/// Child unreaped and forbids replacement. No Child::kill (which also reaps) before census.
pub async fn stop_owned(child: &mut tokio::process::Child, grace: Duration) -> ReapOutcome {
    let Some(pid) = child.id() else {
        return ReapOutcome::Absent;
    };
    #[cfg(unix)]
    {
        if stop_group(pid, grace, || owned_unreaped(pid)).await != ReapOutcome::Stopped {
            return ReapOutcome::Unverified;
        }
    }
    #[cfg(not(unix))]
    {
        // No portable process-tree ownership primitive here: do not claim a tree stop.
        let _ = (pid, grace);
        return ReapOutcome::Unverified;
    }
    #[cfg(unix)]
    match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
        Ok(Ok(_)) => ReapOutcome::Stopped,
        _ => ReapOutcome::Unverified,
    }
}

/// 存放 agent 紀錄的目錄。
#[derive(Debug, Clone)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    pub fn new(state_dir: &Path) -> std::io::Result<Self> {
        let dir = state_dir.join("agents");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, pid: u32) -> PathBuf {
        self.dir.join(format!("{pid}.json"))
    }

    /// 記錄剛啟動的 agent。查不到出生時間（已經退出）時不記錄。
    pub fn record(&self, pid: u32, command: &str) -> std::io::Result<()> {
        let Some(started) = start_time(pid) else {
            return Ok(());
        };
        let rec = AgentRecord {
            pid,
            started,
            command: command.into(),
        };
        let tmp = self.dir.join(format!(".{pid}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec(&rec)?)?;
        std::fs::rename(tmp, self.path(pid))
    }

    pub fn forget(&self, pid: u32) {
        let _ = std::fs::remove_file(self.path(pid));
    }

    pub fn records(&self) -> Vec<AgentRecord> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return vec![];
        };
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
            .collect()
    }
}

/// 檢查單一紀錄：出生時間相符才終止整組。
pub async fn reap(rec: &AgentRecord, grace: Duration) -> ReapOutcome {
    match start_time(rec.pid) {
        None => ReapOutcome::Absent,
        Some(t) if t == rec.started => {
            #[cfg(unix)]
            {
                stop_group(rec.pid, grace, || async {
                    start_time(rec.pid) == Some(rec.started)
                })
                .await
            }
            #[cfg(not(unix))]
            {
                let _ = grace;
                ReapOutcome::Unverified
            }
        }
        Some(_) => ReapOutcome::Unverified, // pid 已被別的程序重用
    }
}

/// 啟動時清理上次殘留的 agent（同一 instance 已持有獨佔鎖時才可呼叫）。
pub async fn reap_orphans(reg: &Registry, grace: Duration) -> Vec<(AgentRecord, ReapOutcome)> {
    let mut out = Vec::new();
    for rec in reg.records() {
        let o = reap(&rec, grace).await;
        match o {
            ReapOutcome::Stopped | ReapOutcome::Absent => reg.forget(rec.pid),
            // pid 重用：紀錄已無意義，移除但不動那個程序
            ReapOutcome::Unverified if start_time(rec.pid).is_some_and(|t| t != rec.started) => {
                reg.forget(rec.pid)
            }
            ReapOutcome::Unverified => tracing::warn!(
                pid = rec.pid,
                "orphan agent could not be stopped; record kept"
            ),
        }
        out.push((rec, o));
    }
    out
}
