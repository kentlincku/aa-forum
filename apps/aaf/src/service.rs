//! `aaf start／stop／restart／status`：在背景啟停服務、閒置才停、查看狀態。
//! 是否執行中一律以 `var/aaf.lock` 的鎖為準；`var/aaf.run.json` 提供埠與管理者。
//! 由 launchd／systemd 管的 home（run.json 的 managed_by）轉給服務管理器，自己不 spawn、不 kill。

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8111";
pub(crate) const LOG: &str = "aaf-up.log";
/// `start` 時 log 超過這個大小就改名成 `.1`（只留一份）
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
const IDLE_POLL: Duration = Duration::from_secs(5);

/// 執行中服務的資訊（`var/aaf.run.json`）。`aaf.lock` 照舊只寫 pid；這個檔在服務結束後保留
/// （下次 start 沿用 listen），是否執行中以鎖為準。
pub fn write_run_info(home: &Path, listen: &str) -> Result<()> {
    let info = json!({
        "pid": std::process::id(),
        "listen": listen,
        "started_at": now(),
        "exe": std::env::current_exe().ok(),
        "version": env!("CARGO_PKG_VERSION"),
        "managed_by": managed_by(),
    });
    let var = home.join("var");
    let tmp = var.join("aaf.run.json.tmp");
    std::fs::write(&tmp, format!("{info:#}\n"))?;
    std::fs::rename(&tmp, var.join("aaf.run.json"))?;
    Ok(())
}

/// 誰在管這個程序：`aaf service install` 寫進 plist／unit 的 AAF_SERVICE 優先（`launchd:<label>`、
/// `systemd:<unit>`）；其次是 systemd（手寫的 unit 也算，unit 名稱取自 cgroup；系統層級的 unit 記成
/// `systemd-system:<unit>`）；都不是就是 None（前景 up 或 aaf start）。
/// 環境變數會被 agent 等子程序繼承，所以只有管理器回報的主程序 pid 就是自己時才算數。
fn managed_by() -> Option<String> {
    let claimed = match std::env::var("AAF_SERVICE") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            std::env::var_os("INVOCATION_ID")?;
            let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
            let unit = cgroup
                .lines()
                .flat_map(|l| l.rsplit('/'))
                .find(|p| p.ends_with(".service") && !p.starts_with("user@"))?;
            if cgroup.contains("/user@") {
                format!("systemd:{unit}")
            } else {
                format!("systemd-system:{unit}")
            }
        }
    };
    let m = Manager::parse(&claimed)?;
    if m.main_pid() != Some(std::process::id()) {
        tracing::warn!(manager = %claimed, "service manager does not report this process as its main pid; treating as unmanaged");
        return None;
    }
    Some(claimed)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct RunInfo {
    pub pid: u32,
    pub listen: String,
    pub started_at: u64,
    pub exe: Option<PathBuf>,
    pub version: String,
    pub managed_by: Option<String>,
}

pub fn read_run_info(home: &Path) -> Option<RunInfo> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("var/aaf.run.json")).ok()?).ok()?;
    Some(RunInfo {
        pid: v["pid"].as_u64()? as u32,
        listen: v["listen"].as_str()?.to_string(),
        started_at: v["started_at"].as_u64().unwrap_or(0),
        exe: v["exe"].as_str().map(PathBuf::from),
        version: v["version"].as_str().unwrap_or_default().to_string(),
        managed_by: v["managed_by"].as_str().map(str::to_string),
    })
}

/// 執行中的服務：pid 取自鎖檔；run.json 只有 pid 相符時才採用（舊版 aaf 不寫 run.json）。
pub struct Running {
    pub pid: u32,
    pub info: Option<RunInfo>,
}

impl Running {
    pub fn listen(&self) -> Option<&str> {
        self.info.as_ref().map(|i| i.listen.as_str())
    }
    /// 程序啟動時間：run.json 優先，否則問 OS。
    pub fn started_at(&self) -> Option<u64> {
        self.info
            .as_ref()
            .map(|i| i.started_at)
            .or_else(|| aaf_runtime::process::start_time(self.pid))
    }
    /// 轉交的對象：run.json 記的管理者，設定檔還在，而且管理器回報的主程序就是這個 pid
    /// （確認不了就自己處理，並提示）。
    pub fn manager(&self) -> Option<Manager> {
        let m = self
            .info
            .as_ref()
            .and_then(|i| i.managed_by.as_deref())
            .and_then(Manager::parse)?;
        if m.installed() && m.main_pid() == Some(self.pid) {
            return Some(m);
        }
        eprintln!(
            "note: var/aaf.run.json says {} but it could not be confirmed (removed, or not running pid {}); handling the process directly",
            m.describe(),
            self.pid
        );
        None
    }
}

/// `aaf service install` 記下的管理者（`var/aaf.service`），設定檔還在才算。
pub fn installed_service(home: &Path) -> Option<Manager> {
    let tag = std::fs::read_to_string(home.join("var").join(SERVICE_FILE)).ok()?;
    Manager::parse(tag.trim()).filter(Manager::installed)
}

pub(crate) const SERVICE_FILE: &str = "aaf.service";

pub fn running(home: &Path) -> Option<Running> {
    let pid: u32 = crate::doctor::lock_held(&home.join("var/aaf.lock"))?
        .parse()
        .ok()?;
    let info = read_run_info(home).filter(|i| i.pid == pid);
    Some(Running { pid, info })
}

/// 試著拿 instance 鎖但不改鎖檔內容（stop 收尾清孤兒用）；拿不到回 None。
fn try_lock(home: &Path) -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.join("var/aaf.lock"))
        .ok()?;
    f.try_lock().ok()?;
    Some(f)
}

// ---------- 服務管理器 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manager {
    Launchd(String),
    SystemdUser(String),
    SystemdSystem(String),
}

impl Manager {
    pub fn parse(s: &str) -> Option<Self> {
        let (kind, name) = s.split_once(':')?;
        let name = name.trim().to_string();
        if name.is_empty() {
            return None;
        }
        match kind {
            "launchd" => Some(Self::Launchd(name)),
            "systemd" => Some(Self::SystemdUser(name)),
            "systemd-system" => Some(Self::SystemdSystem(name)),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Launchd(l) => format!("launchd {l}"),
            Self::SystemdUser(u) => format!("systemd --user {u}"),
            Self::SystemdSystem(u) => format!("systemd (system) {u}"),
        }
    }

    pub fn launchd_target(label: &str) -> String {
        format!("gui/{}/{label}", nix::unistd::getuid())
    }

    pub fn plist(label: &str) -> Option<PathBuf> {
        Some(
            PathBuf::from(std::env::var_os("HOME")?)
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
        )
    }

    /// 管理器回報的主程序 pid（沒在跑或查不到回 None）。
    pub fn main_pid(&self) -> Option<u32> {
        let target;
        let argv: Vec<&str> = match self {
            Self::Launchd(l) => {
                target = Self::launchd_target(l);
                vec!["launchctl", "print", &target]
            }
            Self::SystemdUser(u) => {
                vec!["systemctl", "--user", "show", "-p", "MainPID", "--value", u]
            }
            Self::SystemdSystem(u) => vec!["systemctl", "show", "-p", "MainPID", "--value", u],
        };
        let out = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let out = String::from_utf8_lossy(&out.stdout);
        let pid = match self {
            Self::Launchd(_) => launchd_pid(&out),
            _ => out.trim().parse().ok(),
        };
        pid.filter(|p: &u32| *p > 0)
    }

    /// job 還在管理器手上（launchd 已載入；systemd 是 active／activating，含重啟等待中），
    /// 不論這一刻有沒有程序。
    pub fn active(&self) -> bool {
        match self {
            Self::Launchd(l) => quiet(&["launchctl", "print", &Self::launchd_target(l)]),
            Self::SystemdUser(u) | Self::SystemdSystem(u) => {
                let mut argv = vec!["systemctl"];
                if matches!(self, Self::SystemdUser(_)) {
                    argv.push("--user");
                }
                argv.extend(["show", "-p", "ActiveState", "--value", u]);
                std::process::Command::new(argv[0])
                    .args(&argv[1..])
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .is_some_and(|o| {
                        matches!(
                            String::from_utf8_lossy(&o.stdout).trim(),
                            "active" | "activating" | "reloading" | "deactivating"
                        )
                    })
            }
        }
    }

    /// 設定檔還在（plist 存在／systemctl 找得到 unit）；移除後就不再轉交。
    pub fn installed(&self) -> bool {
        match self {
            Self::Launchd(l) => Self::plist(l).is_some_and(|p| p.exists()),
            Self::SystemdUser(u) => quiet(&["systemctl", "--user", "cat", u]),
            Self::SystemdSystem(u) => quiet(&["systemctl", "cat", u]),
        }
    }

    pub fn refuse_system(&self, verb: &str) -> Result<()> {
        if let Self::SystemdSystem(u) = self {
            bail!("managed by the system unit {u}; run: sudo systemctl {verb} {u}");
        }
        Ok(())
    }

    fn start(&self) -> Result<()> {
        self.refuse_system("start")?;
        match self {
            Self::Launchd(l) => {
                // 已載入就 kickstart；被 stop（bootout）過就重新 bootstrap（RunAtLoad 會啟動）
                let target = Self::launchd_target(l);
                if quiet(&["launchctl", "print", &target]) {
                    run(&["launchctl", "kickstart", &target])
                } else {
                    let plist = Self::plist(l).context("no HOME")?;
                    run(&[
                        "launchctl",
                        "bootstrap",
                        &format!("gui/{}", nix::unistd::getuid()),
                        &plist.to_string_lossy(),
                    ])
                }
            }
            Self::SystemdUser(u) => run(&["systemctl", "--user", "start", u]),
            Self::SystemdSystem(_) => unreachable!(),
        }
    }

    fn stop(&self) -> Result<()> {
        self.refuse_system("stop")?;
        match self {
            Self::Launchd(l) => run(&["launchctl", "bootout", &Self::launchd_target(l)]),
            Self::SystemdUser(u) => run(&["systemctl", "--user", "stop", u]),
            Self::SystemdSystem(_) => unreachable!(),
        }
    }

    fn restart(&self) -> Result<()> {
        self.refuse_system("restart")?;
        match self {
            Self::Launchd(l) => run(&["launchctl", "kickstart", "-k", &Self::launchd_target(l)]),
            Self::SystemdUser(u) => run(&["systemctl", "--user", "restart", u]),
            Self::SystemdSystem(_) => unreachable!(),
        }
    }
}

/// `launchctl print` 的頂層（一個 tab 縮排）有一行「pid = N」；巢狀區塊不算。
fn launchd_pid(out: &str) -> Option<u32> {
    out.lines()
        .find_map(|l| l.strip_prefix("\tpid = "))
        .and_then(|p| p.trim().parse().ok())
}

pub(crate) fn quiet(argv: &[&str]) -> bool {
    std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub(crate) fn run(argv: &[&str]) -> Result<()> {
    eprintln!("$ {}", argv.join(" "));
    let st = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .status()
        .with_context(|| format!("cannot run {}", argv[0]))?;
    if !st.success() {
        bail!("{} failed ({st})", argv.join(" "));
    }
    Ok(())
}

// ---------- 進行中的工作 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Work {
    pub role: String,
    pub room: Option<i64>,
    /// turn＝回合進行中；attempt＝有未提交的 attempt（現在停掉，lane 會卡住要 owner 解除）
    pub kind: &'static str,
    pub started_at: Option<i64>,
}

/// 進行中的工作（唯讀查資料庫）：
/// (a) `context_attempts.state='inflight'`：重啟後一定會卡住的 lane；
/// (b) `agents_turns.outcome='running'` 且在本次服務啟動之後開始（排除被 kill 留下的舊列）。
pub fn in_progress(home: &Path, since: Option<u64>) -> Result<Vec<Work>> {
    let p = home.join("aaf.sqlite3");
    if !p.exists() {
        return Ok(Vec::new());
    }
    let c = open_ro(&p)?;
    let mut out: Vec<Work> = Vec::new();
    if has_table(&c, "agents_turns")? {
        let mut st = c.prepare(
            "SELECT role, room, started_at FROM agents_turns WHERE outcome = 'running' AND started_at >= ?1 ORDER BY id",
        )?;
        let rows = st.query_map([since.unwrap_or(0) as i64], |r| {
            Ok(Work {
                role: r.get(0)?,
                room: r.get(1)?,
                kind: "turn",
                started_at: Some(r.get(2)?),
            })
        })?;
        for w in rows {
            out.push(w?);
        }
    }
    if has_table(&c, "context_attempts")? {
        let mut st = c.prepare(
            "SELECT DISTINCT role, room FROM context_attempts WHERE state = 'inflight' ORDER BY role, room",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Work {
                role: r.get(0)?,
                room: r.get(1)?,
                kind: "attempt",
                started_at: None,
            })
        })?;
        for w in rows {
            let w = w?;
            // 同一條 lane 已列為進行中的回合就不重複
            if !out.iter().any(|o| o.role == w.role && o.room == w.room) {
                out.push(w);
            }
        }
    }
    Ok(out)
}

/// 排隊中、還沒開始的訊息數（不算進行中：重啟後會重派）。
fn queued(home: &Path) -> Option<i64> {
    let c = open_ro(&home.join("aaf.sqlite3")).ok()?;
    c.query_row(
        "SELECT count(*) FROM chat_deliveries d JOIN chat_messages m ON m.id = d.message
         JOIN chat_rooms r ON r.id = m.room WHERE d.state = 'queued' AND r.state = 'active'",
        [],
        |r| r.get(0),
    )
    .ok()
}

fn open_ro(p: &Path) -> Result<rusqlite::Connection> {
    let c = rusqlite::Connection::open_with_flags(
        p,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    c.busy_timeout(Duration::from_secs(5))?;
    Ok(c)
}

fn has_table(c: &rusqlite::Connection, name: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

fn print_work(work: &[Work]) {
    for w in work {
        let room = w.room.map(|r| format!("room {r}")).unwrap_or("-".into());
        let what = match (w.kind, w.started_at) {
            ("turn", Some(t)) => format!("turn running {}", ago(now().saturating_sub(t as u64))),
            ("turn", None) => "turn running".into(),
            _ => "uncommitted attempt (lane needs owner recovery after a stop)".into(),
        };
        println!("    {:<12} {:<8} {what}", w.role, room);
    }
}

/// 閒置檢查。`wait` 是 None：只查一次；有值：每 5 秒查一次，連續兩次都閒置才算，超過時間就放棄。
/// 回傳仍在進行的工作（空＝閒置）。
async fn wait_idle(home: &Path, since: Option<u64>, wait: Option<Duration>) -> Result<Vec<Work>> {
    let work = in_progress(home, since)?;
    let Some(limit) = wait else {
        return Ok(work);
    };
    let deadline = tokio::time::Instant::now() + limit;
    let (mut idle, mut last) = (u8::from(work.is_empty()), work.len());
    if !work.is_empty() {
        println!(
            "waiting for {} in progress (up to {})…",
            work.len(),
            ago(limit.as_secs())
        );
    }
    loop {
        if idle >= 2 {
            return Ok(Vec::new());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return in_progress(home, since);
        }
        tokio::time::sleep(IDLE_POLL.min(deadline - now)).await;
        let work = in_progress(home, since)?;
        if work.is_empty() {
            idle += 1;
        } else {
            idle = 0;
            if work.len() != last {
                println!("  still {} in progress…", work.len());
            }
        }
        last = work.len();
    }
}

// ---------- 健康檢查 ----------

/// `GET /api/v1/health` 回 200（不需登入）。
pub fn healthy(listen: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Some(addr) = listen.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return false;
    };
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!("GET /api/v1/health HTTP/1.0\r\nHost: {listen}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    let n = s.read(&mut head).unwrap_or(0);
    let line = String::from_utf8_lossy(&head[..n]);
    line.starts_with("HTTP/1.") && line.get(9..12) == Some("200")
}

// ---------- start ----------

pub(crate) fn absolute(home: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(home.join("var"))?;
    Ok(std::fs::canonicalize(home)?)
}

fn rotate_log(var: &Path) {
    let p = var.join(LOG);
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&p, var.join(format!("{LOG}.1")));
    }
}

fn log_tail(var: &Path, lines: usize) -> String {
    let s = std::fs::read_to_string(var.join(LOG)).unwrap_or_default();
    let v: Vec<&str> = s.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

/// 等服務就緒：鎖被持有、pid 是自己 spawn 的那個（有 child 時）且不是重啟前的舊程序、health 回 200。
pub(crate) async fn wait_ready(
    home: &Path,
    mut child: Option<&mut std::process::Child>,
    old_pid: Option<u32>,
    timeout: Duration,
) -> Result<Option<(u32, String)>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(c) = child.as_deref_mut() {
            if let Some(st) = c.try_wait()? {
                bail!(
                    "aaf exited during startup ({st}). Last lines of {}:\n{}",
                    home.join("var").join(LOG).display(),
                    log_tail(&home.join("var"), 30)
                );
            }
        }
        if let Some(r) = running(home) {
            let want = child.as_deref().map(|c| c.id());
            let pid_ok = want.is_none_or(|w| w == r.pid) && old_pid != Some(r.pid);
            if let (true, Some(listen)) = (pid_ok, r.listen()) {
                if healthy(listen) {
                    return Ok(Some((r.pid, listen.to_string())));
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn start(home: &Path, listen: Option<String>, timeout: Duration) -> Result<i32> {
    let home = absolute(home)?;
    if let Some(r) = running(&home) {
        match r.listen() {
            Some(l) => println!("already running (pid {}) on http://{l}/app/", r.pid),
            None => println!("already running (pid {})", r.pid),
        }
        return Ok(0);
    }
    let last = read_run_info(&home);
    let var = home.join("var");
    // 裝成服務了（service install 的紀錄，或上次由手寫 unit 啟動）、設定檔也還在：轉給它
    if let Some(m) = installed_service(&home).or_else(|| {
        last.as_ref()
            .and_then(|i| i.managed_by.as_deref())
            .and_then(Manager::parse)
            .filter(Manager::installed)
    }) {
        if listen.is_some() {
            eprintln!(
                "note: --listen is ignored; the address comes from the {} configuration",
                m.describe()
            );
        }
        m.start()?;
        return report_ready(
            &home,
            wait_ready(&home, None, None, timeout).await?,
            timeout,
        );
    }
    let listen = listen
        .or(last.map(|i| i.listen))
        .unwrap_or_else(|| DEFAULT_LISTEN.into());
    rotate_log(&var);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(var.join(LOG))?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .arg("--home")
        .arg(&home)
        .args(["up", "--listen", &listen, "--detach"])
        // 呼叫端若是服務底下的 agent，這兩個變數是繼承來的：不能讓這個實例被當成那個服務
        .env_remove("AAF_SERVICE")
        .env_remove("INVOCATION_ID")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("cannot start aaf up")?;
    let ready = wait_ready(&home, Some(&mut child), None, timeout).await?;
    // 逾時時子程序留著（啟動慢不該被殺）；它已脫離本程序的工作階段
    report_ready(&home, ready, timeout)
}

pub(crate) fn report_ready(
    home: &Path,
    ready: Option<(u32, String)>,
    timeout: Duration,
) -> Result<i32> {
    match ready {
        Some((pid, listen)) => {
            println!("started (pid {pid}) on http://{listen}/app/");
            Ok(0)
        }
        None => {
            eprintln!(
                "not ready after {}s; it may still be starting. Check `aaf --home {} status` or stop it with `aaf stop`. Last lines of the log:\n{}",
                timeout.as_secs(),
                home.display(),
                log_tail(&home.join("var"), 30)
            );
            Ok(1)
        }
    }
}

// ---------- stop ----------

pub struct StopOpts {
    pub force: bool,
    pub wait: Option<Duration>,
    pub timeout: Duration,
    pub kill: bool,
}

/// 結束碼：0 已停（或本來就沒在跑）、2 有進行中的工作而拒絕、1 錯誤或逾時。
pub async fn stop(home: &Path, o: &StopOpts) -> Result<i32> {
    let home = absolute(home)?;
    let Some(r) = running(&home) else {
        // 服務的 job 在跑、實例卻沒起來（例如卡在啟動）：也要停得掉
        if let Some(m) = installed_service(&home).filter(Manager::active) {
            m.stop()?;
            println!("stopped the {} job (the instance was not up)", m.describe());
            return Ok(0);
        }
        println!("not running");
        return Ok(0);
    };
    let mgr = r.manager();
    if let Some(m) = &mgr {
        m.refuse_system("stop")?;
    }
    if let Some(code) = refuse_if_busy(&home, &r, o).await? {
        return Ok(code);
    }
    stop_running(&home, &r, mgr, o).await
}

/// 沒加 --force 時的閒置檢查；有工作在跑就印出清單並回傳結束碼 2。
pub(crate) async fn refuse_if_busy(home: &Path, r: &Running, o: &StopOpts) -> Result<Option<i32>> {
    if o.force {
        return Ok(None);
    }
    let work = wait_idle(home, r.started_at(), o.wait).await?;
    if work.is_empty() {
        return Ok(None);
    }
    eprintln!(
        "refusing: {} in progress (wait with --wait, or interrupt them with --force):",
        work.len()
    );
    print_work(&work);
    Ok(Some(2))
}

/// 確認鎖仍由同一個 pid 持有（等閒置可能等了很久）。已停回 None；換了程序就報錯，不對舊 pid 送訊號。
fn still_same(home: &Path, r: &Running) -> Result<Option<Running>> {
    match running(home) {
        None => Ok(None),
        Some(now) if now.pid == r.pid => Ok(Some(now)),
        Some(now) => bail!(
            "the service was restarted meanwhile (pid {} -> {}); run the command again",
            r.pid,
            now.pid
        ),
    }
}

/// 只在鎖仍由這個 pid 持有時送訊號；程序剛好自己結束（ESRCH）視同已停。
fn signal(home: &Path, r: &Running, sig: nix::sys::signal::Signal) -> Result<()> {
    if still_same(home, r)?.is_none() {
        return Ok(());
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(r.pid as i32), sig) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot signal pid {}", r.pid)),
    }
}

async fn stop_running(home: &Path, r: &Running, mgr: Option<Manager>, o: &StopOpts) -> Result<i32> {
    let Some(r) = still_same(home, r)? else {
        println!("stopped (it exited meanwhile)");
        return Ok(0);
    };
    match &mgr {
        Some(m) => m.stop()?,
        None => signal(home, &r, nix::sys::signal::Signal::SIGTERM)?,
    }
    if !wait_released(home, o.timeout).await {
        if o.kill && mgr.is_none() {
            eprintln!(
                "still running after {}s; sending SIGKILL to pid {}",
                o.timeout.as_secs(),
                r.pid
            );
            signal(home, &r, nix::sys::signal::Signal::SIGKILL)?;
            if !wait_released(home, Duration::from_secs(5)).await {
                bail!("pid {} did not exit after SIGKILL", r.pid);
            }
        } else {
            eprintln!(
                "still stopping after {}s (pid {}); check `aaf status`, or use --kill",
                o.timeout.as_secs(),
                r.pid
            );
            return Ok(1);
        }
    }
    reap(home).await?;
    println!("stopped");
    Ok(0)
}

/// 保險：服務沒停乾淨（舊版被 SIGTERM 直接殺、或 --kill）留下的 agent，比對身分後清掉。
/// 拿不到鎖（又有人啟動了）就不動。
pub(crate) async fn reap(home: &Path) -> Result<()> {
    if let Some(_lock) = try_lock(home) {
        let reg = aaf_runtime::process::Registry::new(&home.join("var"))?;
        for (rec, outcome) in aaf_runtime::process::reap_orphans(&reg, Duration::from_secs(3)).await
        {
            eprintln!(
                "orphan agent pid {} ({}): {outcome:?}",
                rec.pid, rec.command
            );
        }
    }
    Ok(())
}

pub(crate) async fn wait_released(home: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if running(home).is_none() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------- restart ----------

pub async fn restart(
    home: &Path,
    o: &StopOpts,
    listen: Option<String>,
    start_timeout: Duration,
) -> Result<i32> {
    let home = absolute(home)?;
    let Some(r) = running(&home) else {
        return start(&home, listen, start_timeout).await;
    };
    let mgr = r.manager();
    if let Some(m) = &mgr {
        m.refuse_system("restart")?;
    }
    if let Some(code) = refuse_if_busy(&home, &r, o).await? {
        return Ok(code);
    }
    // 等閒置的期間服務可能已經換了程序：不要對舊 pid 下手
    let Some(r) = still_same(&home, &r)? else {
        return start(&home, listen, start_timeout).await;
    };
    if let Some(m) = mgr {
        if listen.is_some() {
            eprintln!(
                "note: --listen is ignored; the address comes from the {} configuration",
                m.describe()
            );
        }
        m.restart()?;
        return report_ready(
            &home,
            wait_ready(&home, None, Some(r.pid), start_timeout).await?,
            start_timeout,
        );
    }
    let Some(listen) = listen.or_else(|| r.listen().map(str::to_string)) else {
        bail!(
            "pid {} was started by an older aaf that does not record its address; pass --listen",
            r.pid
        );
    };
    match stop_running(&home, &r, None, o).await? {
        0 => start(&home, Some(listen), start_timeout).await,
        code => Ok(code),
    }
}

// ---------- status ----------

fn ago(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m {s}s"),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// 結束碼：0 執行中、3 已停（LSB 慣例）。
pub fn status(home: &Path, as_json: bool) -> Result<i32> {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let log = home.join("var").join(LOG);
    let Some(r) = running(&home) else {
        let last = read_run_info(&home);
        if as_json {
            println!(
                "{:#}",
                json!({"running": false, "home": home, "last_listen": last.map(|i| i.listen), "log": log})
            );
        } else {
            println!("stopped  (home {})", home.display());
        }
        return Ok(3);
    };
    let started = r.started_at();
    let uptime = started.map(|t| now().saturating_sub(t));
    let exe = r.info.as_ref().and_then(|i| i.exe.clone());
    // 執行檔在程序啟動後被換掉（重新編譯）：重啟才會用到新版
    let rebuilt = match (&exe, started) {
        (Some(p), Some(t)) => std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|m| m.as_secs() > t),
        _ => false,
    };
    let health = r.listen().map(healthy);
    let work = in_progress(&home, started)?;
    let queued = queued(&home);
    let managed = r.info.as_ref().and_then(|i| i.managed_by.clone());
    if as_json {
        println!(
            "{:#}",
            json!({
                "running": true,
                "home": home,
                "pid": r.pid,
                "listen": r.listen(),
                "started_at": started,
                "uptime_secs": uptime,
                "version": r.info.as_ref().map(|i| &i.version),
                "exe": exe,
                "rebuilt_since_start": rebuilt,
                "managed_by": managed,
                "healthy": health,
                "in_progress": work.iter().map(|w| json!({"role": w.role, "room": w.room, "kind": w.kind, "started_at": w.started_at})).collect::<Vec<_>>(),
                "queued": queued,
                "idle": work.is_empty(),
                "log": log,
            })
        );
        return Ok(0);
    }
    let url = r
        .listen()
        .map(|l| format!("http://{l}/app/"))
        .unwrap_or_else(|| "(address unknown: started by an older aaf)".into());
    println!(
        "running  pid {}  {url}  up {}",
        r.pid,
        uptime.map(ago).unwrap_or("?".into())
    );
    println!("  home     {}", home.display());
    if let Some(i) = &r.info {
        let exe = i
            .exe
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let note = if rebuilt {
            "  (rebuilt since start: restart to use it)"
        } else {
            ""
        };
        println!("  version  {}  {exe}{note}", i.version);
    }
    println!(
        "  managed  {}",
        managed
            .as_deref()
            .and_then(Manager::parse)
            .map(|m| m.describe())
            .or(managed.clone())
            .unwrap_or("no (aaf start or aaf up)".into())
    );
    println!("  log      {}", log.display());
    match (health, r.listen()) {
        (Some(true), _) => println!("  health   ok"),
        (Some(false), Some(l)) => println!("  health   no answer on {l}"),
        _ => {}
    }
    if work.is_empty() {
        println!("  work     idle: safe to restart");
    } else {
        println!("  work     {} in progress:", work.len());
        print_work(&work);
    }
    if let Some(q) = queued.filter(|q| *q > 0) {
        println!("  queued   {q} messages waiting (redelivered after a restart)");
    }
    Ok(0)
}

const MAX_DURATION_SECS: u64 = 7 * 24 * 3600;

/// `--timeout`／`--wait` 的時間：`90`、`90s`、`10m`、`1h`（最多 7 天）。
pub fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    let (num, mul) = match s.char_indices().last() {
        Some((i, 's')) => (&s[..i], 1),
        Some((i, 'm')) => (&s[..i], 60),
        Some((i, 'h')) => (&s[..i], 3600),
        _ => (s, 1),
    };
    let secs = num
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mul))
        .ok_or_else(|| format!("invalid duration {s:?} (use e.g. 30s, 10m, 1h)"))?;
    if secs > MAX_DURATION_SECS {
        return Err(format!("duration {s:?} is too long (at most 7 days)"));
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("18446744073709551615h").is_err());
        assert!(parse_duration("8d").is_err());
        assert!(parse_duration("169h").is_err());
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
    }

    #[test]
    fn launchd_pid_is_top_level_only() {
        let out = "gui/501/io.aaf.x = {\n\tstate = running\n\tendpoints = {\n\t\tpid = 7\n\t}\n\tpid = 2031\n}";
        assert_eq!(launchd_pid(out), Some(2031));
        assert_eq!(launchd_pid("\tstate = not running\n"), None);
    }

    #[test]
    fn managers() {
        assert_eq!(
            Manager::parse("launchd:io.aaf.aaf-alice"),
            Some(Manager::Launchd("io.aaf.aaf-alice".into()))
        );
        assert_eq!(
            Manager::parse("systemd:aaf-alice.service"),
            Some(Manager::SystemdUser("aaf-alice.service".into()))
        );
        assert_eq!(
            Manager::parse("systemd-system:aaf.service"),
            Some(Manager::SystemdSystem("aaf.service".into()))
        );
        // 只知道是 systemd、不知道 unit：無法轉交
        assert_eq!(Manager::parse("systemd"), None);
        assert_eq!(Manager::parse("launchd:"), None);
    }

    #[test]
    fn in_progress_ignores_stale_turns_and_dedups_lanes() {
        let home = tempfile::tempdir().unwrap();
        let c = rusqlite::Connection::open(home.path().join("aaf.sqlite3")).unwrap();
        c.execute_batch(
            "CREATE TABLE agents_turns(id INTEGER PRIMARY KEY, role TEXT, room INTEGER, started_at INTEGER, outcome TEXT);
             CREATE TABLE context_attempts(id INTEGER PRIMARY KEY, role TEXT, room INTEGER, state TEXT);
             INSERT INTO agents_turns(role, room, started_at, outcome) VALUES
               ('old', 1, 100, 'running'), ('planner', 6, 2000, 'running'), ('done', 2, 2000, 'ok');
             INSERT INTO context_attempts(role, room, state) VALUES
               ('planner', 6, 'inflight'), ('designer', 2, 'inflight'), ('scribe', 3, 'checkpointed');",
        )
        .unwrap();
        let w = in_progress(home.path(), Some(1000)).unwrap();
        let got: Vec<(&str, Option<i64>, &str)> = w
            .iter()
            .map(|w| (w.role.as_str(), w.room, w.kind))
            .collect();
        assert_eq!(
            got,
            vec![
                ("planner", Some(6), "turn"),
                ("designer", Some(2), "attempt")
            ]
        );
        // 沒有資料庫：閒置
        let empty = tempfile::tempdir().unwrap();
        assert!(in_progress(empty.path(), None).unwrap().is_empty());
    }
}
