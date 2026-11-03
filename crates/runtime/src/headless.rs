//! headless：每輪執行一次 agent 自己的非互動 CLI（`claude -p`、`codex exec`、`pi -p`），提示走 stdin，
//! 逐行解析 JSONL 輸出。不經 ACP 轉接器；session 以 CLI 自己的 id 續接（claude／pi 開場就指定 id，codex 用回報的 thread id）。
//! 沒有權限卡片：權限在啟動參數決定（`@widest`＝全放行，其餘見 `permission_args`）。

use crate::driver::*;
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cli {
    Claude,
    Codex,
    Pi,
}

impl Cli {
    /// 目錄 agent id → 支援 headless 的 CLI；其他 agent 照常走 ACP。
    pub fn from_agent(agent: &str) -> Option<Self> {
        match agent {
            "claude" => Some(Cli::Claude),
            "codex" => Some(Cli::Codex),
            "pi" => Some(Cli::Pi),
            _ => None,
        }
    }
    pub fn program(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
            Cli::Pi => "pi",
        }
    }
    /// 回報給 continuity 的名稱（決定換 session 門檻：含 claude／codex）。
    fn agent_name(self) -> &'static str {
        match self {
            Cli::Claude => "claude-headless",
            Cli::Codex => "codex-headless",
            Cli::Pi => "pi-headless",
        }
    }
    /// claude／pi 可以開場就指定 session id；codex 要等 thread.started 回報。
    fn presets_session(self) -> bool {
        !matches!(self, Cli::Codex)
    }
}

pub struct HeadlessDriver {
    cli: Cli,
    program: String,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    model: Option<String>,
    mode: Option<String>,
    effort: Option<String>,
    sig: Signals,
    session: std::sync::Mutex<Option<String>>,
    /// 指定了 id 但 CLI 還沒建立過這個 session（claude 第一輪用 --session-id，之後用 --resume）
    fresh: AtomicBool,
    resume: std::sync::Mutex<Option<String>>,
    loaded: AtomicBool,
    usage: std::sync::Mutex<Option<(u64, u64)>>,
    /// 最近一個 rate_limit_event 的資訊和收到的時間（claude）
    rate: std::sync::Mutex<Option<(f64, Value)>>,
    account: Option<String>,
    /// 上下文窗口大小（codex／pi 開場查一次；claude 每輪從 result 拿）
    window: std::sync::Mutex<Option<u64>>,
    pid: std::sync::Mutex<Option<u32>>,
    cancel: tokio::sync::Notify,
    registry: Option<crate::process::Registry>,
}

impl HeadlessDriver {
    pub fn new(cli: Cli, program: String, cwd: PathBuf, env: Vec<(String, String)>) -> Self {
        Self {
            cli,
            program,
            cwd,
            env,
            model: None,
            mode: None,
            effort: None,
            sig: Signals::new(AgentStatus::Offline),
            session: Default::default(),
            fresh: Default::default(),
            resume: Default::default(),
            loaded: Default::default(),
            usage: Default::default(),
            rate: Default::default(),
            account: None,
            window: Default::default(),
            pid: Default::default(),
            cancel: tokio::sync::Notify::new(),
            registry: None,
        }
    }
    pub fn with_model(mut self, m: Option<String>) -> Self {
        self.model = m.filter(|m| !m.is_empty());
        self
    }
    pub fn with_account(mut self, a: Option<String>) -> Self {
        self.account = a.filter(|a| !a.is_empty());
        self
    }
    pub fn with_effort(mut self, e: Option<String>) -> Self {
        self.effort = e.filter(|e| !e.is_empty());
        self
    }
    pub fn with_mode(mut self, m: Option<String>) -> Self {
        self.mode = m.filter(|m| !m.is_empty());
        self
    }
    pub fn with_resume(self, session_id: Option<String>) -> Self {
        *self.resume.lock().unwrap() = session_id.filter(|s| !s.is_empty());
        self
    }
    pub fn with_registry(mut self, r: Option<crate::process::Registry>) -> Self {
        self.registry = r;
        self
    }

    /// 權限在啟動參數決定。`@widest`＝群開了 YOLO。沒指定時：claude 自動接受編輯、只放行 `aaf` 指令；
    /// codex 可寫工作目錄、可連網（aaf CLI 走本機 API）；pi 本身沒有權限詢問。
    fn permission_args(&self) -> Vec<String> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        match (self.cli, self.mode.as_deref()) {
            (Cli::Claude, Some(crate::catalog::WIDEST)) => s(&["--dangerously-skip-permissions"]),
            (Cli::Claude, Some(m)) => s(&["--permission-mode", m]),
            (Cli::Claude, None) => s(&[
                "--permission-mode",
                "acceptEdits",
                "--allowedTools",
                "Bash(aaf:*)",
            ]),
            (Cli::Codex, Some(crate::catalog::WIDEST)) => {
                s(&["--dangerously-bypass-approvals-and-sandbox"])
            }
            (Cli::Codex, Some(m)) => s(&["--sandbox", m]),
            (Cli::Codex, None) => s(&[
                "--sandbox",
                "workspace-write",
                "-c",
                "sandbox_workspace_write.network_access=true",
            ]),
            (Cli::Pi, _) => vec![],
        }
    }

    /// 這一輪的完整參數（提示另走 stdin）。
    pub fn args(&self) -> Vec<String> {
        let session = self.session.lock().unwrap().clone();
        let mut a: Vec<String> = vec![];
        match self.cli {
            Cli::Claude => {
                a.extend(["-p", "--output-format", "stream-json", "--verbose"].map(String::from));
                if let Some(m) = &self.model {
                    a.extend(["--model".into(), m.clone()]);
                }
                if let Some(e) = &self.effort {
                    a.extend(["--effort".into(), e.clone()]);
                }
                a.extend(self.permission_args());
                if let Some(id) = session {
                    let flag = if self.fresh.load(Ordering::SeqCst) {
                        "--session-id"
                    } else {
                        "--resume"
                    };
                    a.extend([flag.into(), id]);
                }
            }
            Cli::Codex => {
                a.extend(["exec", "--json", "--skip-git-repo-check", "-C"].map(String::from));
                a.push(self.cwd.display().to_string());
                if let Some(m) = &self.model {
                    a.extend(["-m".into(), m.clone()]);
                }
                if let Some(e) = &self.effort {
                    a.extend(["-c".into(), format!("model_reasoning_effort={e}")]);
                }
                a.extend(self.permission_args());
                if let Some(id) = session {
                    a.extend(["resume".into(), id]);
                }
                a.push("-".into());
            }
            Cli::Pi => {
                a.extend(["-p", "--mode", "json"].map(String::from));
                if let Some(m) = &self.model {
                    a.extend(["--model".into(), m.clone()]);
                }
                if let Some(e) = &self.effort {
                    a.extend(["--thinking".into(), e.clone()]);
                }
                if let Some(id) = session {
                    a.extend(["--session-id".into(), id]);
                }
            }
        }
        a
    }
}

/// 一輪輸出的累積狀態。
#[derive(Default)]
struct Turn {
    text: String,
    stop: Option<String>,
    error: Option<String>,
    used: Option<u64>,
    window: Option<u64>,
    session: Option<String>,
}

fn tokens(v: &Value, keys: &[&str]) -> u64 {
    keys.iter().filter_map(|k| v[*k].as_u64()).sum()
}

/// 解析一行 JSONL；要發的事件回傳出去。格式見各 CLI 的 headless 文件（stream-json／exec --json／--mode json）。
fn parse_line(cli: Cli, d: &Value, t: &mut Turn) -> Vec<DriverEvent> {
    let mut ev = vec![];
    let tool = |id: &Value, title: String, status: &str| DriverEvent::ToolCall {
        id: id.as_str().unwrap_or_default().into(),
        title,
        status: status.into(),
    };
    match cli {
        Cli::Claude => match d["type"].as_str() {
            Some("system") if d["subtype"] == "init" => {
                t.session = d["session_id"].as_str().map(String::from);
            }
            Some("assistant") => {
                let m = &d["message"];
                for c in m["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str() {
                        Some("text") => {
                            if let Some(x) = c["text"].as_str() {
                                ev.push(DriverEvent::MessageChunk { text: x.into() });
                            }
                        }
                        Some("tool_use") => ev.push(tool(
                            &c["id"],
                            c["name"].as_str().unwrap_or("tool").into(),
                            "in_progress",
                        )),
                        _ => {}
                    }
                }
                if m["usage"].is_object() {
                    t.used = Some(tokens(
                        &m["usage"],
                        &[
                            "input_tokens",
                            "cache_creation_input_tokens",
                            "cache_read_input_tokens",
                            "output_tokens",
                        ],
                    ));
                }
            }
            Some("user") => {
                for c in d["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] == "tool_result" {
                        let st = if c["is_error"] == true {
                            "failed"
                        } else {
                            "completed"
                        };
                        ev.push(tool(&c["tool_use_id"], "tool result".into(), st));
                    }
                }
            }
            Some("result") => {
                t.text = d["result"].as_str().unwrap_or_default().into();
                t.window = d["modelUsage"]
                    .as_object()
                    .and_then(|m| m.values().filter_map(|x| x["contextWindow"].as_u64()).max());
                if d["is_error"] == true || d["subtype"].as_str().is_some_and(|s| s != "success") {
                    t.error = Some(
                        d["result"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or(d["subtype"].as_str())
                            .unwrap_or("error")
                            .into(),
                    );
                } else {
                    t.stop = Some(match d["stop_reason"].as_str() {
                        Some("max_tokens") => "max_tokens".into(),
                        _ => "end_turn".into(),
                    });
                }
            }
            _ => {}
        },
        Cli::Codex => match d["type"].as_str() {
            Some("thread.started") => t.session = d["thread_id"].as_str().map(String::from),
            Some(k @ ("item.started" | "item.completed")) => {
                let it = &d["item"];
                match it["type"].as_str() {
                    Some("agent_message") if k == "item.completed" => {
                        let x = it["text"].as_str().unwrap_or_default();
                        ev.push(DriverEvent::MessageChunk { text: x.into() });
                        t.text = x.into();
                    }
                    Some("agent_message" | "reasoning") | None => {}
                    Some(other) => {
                        let title = it["command"].as_str().unwrap_or(other).to_string();
                        let st = if k == "item.started" {
                            "in_progress"
                        } else {
                            "completed"
                        };
                        ev.push(tool(&it["id"], title, st));
                    }
                }
            }
            Some("turn.completed") => {
                t.used = Some(tokens(&d["usage"], &["input_tokens", "output_tokens"]));
                t.stop = Some("end_turn".into());
            }
            Some("turn.failed") | Some("error") => {
                let m = d["error"]["message"]
                    .as_str()
                    .or(d["message"].as_str())
                    .unwrap_or("codex error");
                t.error = Some(m.into());
            }
            _ => {}
        },
        Cli::Pi => match d["type"].as_str() {
            Some("session") => t.session = d["id"].as_str().map(String::from),
            Some("message_end") if d["message"]["role"] == "assistant" => {
                let m = &d["message"];
                let x: String = m["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect();
                let x = x.trim().to_string();
                if !x.is_empty() {
                    ev.push(DriverEvent::MessageChunk { text: x.clone() });
                    t.text = x;
                }
                if m["usage"].is_object() {
                    t.used = Some(m["usage"]["totalTokens"].as_u64().unwrap_or_else(|| {
                        tokens(&m["usage"], &["input", "output", "cacheRead", "cacheWrite"])
                    }));
                }
                match m["stopReason"].as_str() {
                    Some("stop") | Some("toolUse") | None => {}
                    Some("length") => t.stop = Some("max_tokens".into()),
                    Some("aborted") => t.stop = Some("cancelled".into()),
                    Some(other) => {
                        t.error = Some(m["errorMessage"].as_str().unwrap_or(other).to_string())
                    }
                }
            }
            Some(k) if k.starts_with("tool_execution") => {
                let st = if k.ends_with("end") {
                    if d["isError"] == true {
                        "failed"
                    } else {
                        "completed"
                    }
                } else {
                    "in_progress"
                };
                let title = d["toolName"].as_str().unwrap_or("tool").to_string();
                ev.push(tool(&d["toolCallId"], title, st));
            }
            Some("agent_settled") => {
                if d["aborted"] == true {
                    t.stop = Some("cancelled".into());
                } else if t.stop.is_none() && t.error.is_none() {
                    t.stop = Some("end_turn".into());
                }
            }
            _ => {}
        },
    }
    ev
}

fn random_uuid() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_err()
    {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        b = (n ^ (std::process::id() as u128).rotate_left(64)).to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// codex：`$CODEX_HOME/models_cache.json` 的 context_window（codex 實際只用 95%）；沒指定模型時看 config.toml。
fn codex_window(model: Option<&str>) -> Option<u64> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".codex")))?;
    let model = match model {
        Some(m) => m.to_string(),
        None => {
            let cfg = std::fs::read_to_string(home.join("config.toml")).ok()?;
            cfg.lines()
                .find_map(|l| l.trim().strip_prefix("model").map(str::trim))
                .and_then(|l| l.strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_string())?
        }
    };
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("models_cache.json")).ok()?)
            .ok()?;
    let w = v["models"]
        .as_array()?
        .iter()
        .find(|m| m["slug"] == model.as_str() || m["id"] == model.as_str())?["context_window"]
        .as_u64()?;
    Some(w * 95 / 100)
}

/// pi：`~/.pi/agent/models.json` 的 contextWindow；找不到就問 `pi --list-models`（不花 token）。
async fn pi_window(program: &str, model: Option<&str>) -> Option<u64> {
    let model = model?;
    let (provider, id) = model.split_once('/').unwrap_or(("", model));
    if let Some(home) = std::env::var_os("HOME") {
        let p = Path::new(&home).join(".pi/agent/models.json");
        if let Some(v) = std::fs::read_to_string(p)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            for (name, prov) in v["providers"].as_object().into_iter().flatten() {
                if !provider.is_empty() && name != provider {
                    continue;
                }
                for m in prov["models"].as_array().into_iter().flatten() {
                    if m["id"] == id {
                        if let Some(w) = m["contextWindow"].as_u64() {
                            return Some(w);
                        }
                    }
                }
            }
        }
    }
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(program)
            .args(["--list-models", id])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let row = text.lines().find(|l| {
        let c: Vec<&str> = l.split_whitespace().collect();
        c.len() > 2 && c[1] == id && (provider.is_empty() || c[0] == provider)
    })?;
    parse_size(row.split_whitespace().nth(2)?)
}

/// "131.1K"／"1M"／"200000" → token 數。
fn parse_size(s: &str) -> Option<u64> {
    let (n, mul) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1_000.0),
        'M' | 'm' => (&s[..s.len() - 1], 1_000_000.0),
        _ => (s, 1.0),
    };
    Some((n.parse::<f64>().ok()? * mul) as u64)
}

#[async_trait]
impl Driver for HeadlessDriver {
    fn kind(&self) -> &'static str {
        "headless"
    }
    async fn start(&self) -> DResult<()> {
        if crate::catalog::which(&self.program).is_none() && !Path::new(&self.program).is_file() {
            return Err(DriverError::Protocol(format!(
                "{} not found (headless needs the {} CLI on PATH)",
                self.program,
                self.cli.program()
            )));
        }
        let resume = self.resume.lock().unwrap().clone();
        let (session, fresh) = match resume {
            Some(id) => (Some(id), false),
            None if self.cli.presets_session() => (Some(random_uuid()), true),
            None => (None, false),
        };
        self.loaded
            .store(session.is_some() && !fresh, Ordering::SeqCst);
        *self.session.lock().unwrap() = session;
        self.fresh.store(fresh, Ordering::SeqCst);
        *self.usage.lock().unwrap() = None;
        let window = match self.cli {
            Cli::Claude => None,
            Cli::Codex => codex_window(self.model.as_deref()),
            Cli::Pi => pi_window(&self.program, self.model.as_deref()).await,
        };
        *self.window.lock().unwrap() = window;
        self.sig.set(AgentStatus::Idle);
        Ok(())
    }
    async fn prompt(&self, text: &str) -> DResult<TurnResult> {
        if self.sig.get() == AgentStatus::Working {
            return Err(DriverError::Busy);
        }
        if self.sig.get() == AgentStatus::Offline {
            return Err(DriverError::NotRunning);
        }
        self.sig.set(AgentStatus::Working);
        let res = self.run(text).await;
        self.sig.set(AgentStatus::Idle);
        if let Ok(r) = &res {
            self.sig.emit(DriverEvent::TurnEnded {
                stop_reason: r.stop_reason.clone(),
            });
        }
        res
    }
    async fn cancel(&self) -> DResult<()> {
        if self.sig.get() == AgentStatus::Working {
            self.cancel.notify_one();
        }
        Ok(())
    }
    async fn respond_permission(&self, _: u64, _: PermissionDecision) -> DResult<()> {
        Err(DriverError::Unsupported(
            "headless agents have no permission requests; permissions are set at launch",
        ))
    }
    fn status(&self) -> AgentStatus {
        self.sig.get()
    }
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.sig.tx.subscribe()
    }
    async fn stop(&self) -> DResult<()> {
        self.cancel().await?;
        let pid = *self.pid.lock().unwrap();
        if let Some(pid) = pid {
            crate::process::kill_group(pid, std::time::Duration::from_secs(3)).await;
        }
        self.sig.set(AgentStatus::Offline);
        Ok(())
    }
    fn session_id(&self) -> Option<String> {
        self.session.lock().unwrap().clone()
    }
    fn session_loaded(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }
    fn context_usage(&self) -> Option<ContextUsage> {
        let (used, size) = (*self.usage.lock().unwrap())?;
        Some(ContextUsage {
            used,
            size,
            agent: self.cli.agent_name().into(),
        })
    }
    fn rate_limit(&self) -> Option<Value> {
        let (at, info) = self.rate.lock().unwrap().clone()?;
        crate::ratelimit::normalize(&info, self.account.as_deref().unwrap_or("default"), at)
    }
}

impl HeadlessDriver {
    async fn run(&self, text: &str) -> DResult<TurnResult> {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(self.args())
            .current_dir(&self.cwd)
            .envs(self.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::process::isolate(&mut cmd);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        *self.pid.lock().unwrap() = pid;
        if let (Some(r), Some(pid)) = (&self.registry, pid) {
            let _ = r.record(pid, &format!("{} (headless)", self.program));
        }
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(text.as_bytes()).await?;
        drop(stdin);
        let mut stderr = child.stderr.take().expect("piped");
        let err_task = tokio::spawn(async move {
            let mut b = Vec::new();
            let _ = stderr.read_to_end(&mut b).await;
            let s = String::from_utf8_lossy(&b).trim().to_string();
            let start = s.len().saturating_sub(600);
            let start = (start..s.len())
                .find(|i| s.is_char_boundary(*i))
                .unwrap_or(0);
            s[start..].to_string()
        });
        let mut lines = tokio::io::BufReader::new(child.stdout.take().expect("piped")).lines();
        let mut t = Turn::default();
        let cancelled = loop {
            tokio::select! {
                l = lines.next_line() => match l? {
                    Some(l) => {
                        let Ok(d) = serde_json::from_str::<Value>(&l) else { continue };
                        if let Some(info) = crate::ratelimit::from_stream_line(&d) {
                            *self.rate.lock().unwrap() = Some((crate::ratelimit::now_secs(), info.clone()));
                        }
                        for e in parse_line(self.cli, &d, &mut t) {
                            self.sig.emit(e);
                        }
                        if let Some(s) = t.session.take() {
                            *self.session.lock().unwrap() = Some(s);
                            self.fresh.store(false, Ordering::SeqCst);
                        }
                    }
                    None => break false,
                },
                _ = self.cancel.notified() => break true,
            }
        };
        if let (Some(r), Some(pid)) = (&self.registry, pid) {
            r.forget(pid);
        }
        if cancelled {
            if let Some(pid) = pid {
                crate::process::kill_group(pid, std::time::Duration::from_secs(3)).await;
            }
            let _ = child.kill().await;
            *self.pid.lock().unwrap() = None;
            return Ok(TurnResult {
                text: String::new(),
                stop_reason: "cancelled".into(),
            });
        }
        let st = child.wait().await?;
        *self.pid.lock().unwrap() = None;
        let stderr = err_task.await.unwrap_or_default();
        if let Some(w) = t.window {
            *self.window.lock().unwrap() = Some(w);
        }
        if let (Some(used), Some(size)) = (t.used, *self.window.lock().unwrap()) {
            *self.usage.lock().unwrap() = Some((used, size));
        }
        let stop_reason = match (&t.error, &t.stop) {
            (Some(e), _) => format!("error: {e}"),
            (None, Some(s)) => s.clone(),
            (None, None) if !st.success() => format!(
                "error: {} exited with {st}{}",
                self.cli.program(),
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            ),
            (None, None) => "end_turn".into(),
        };
        Ok(TurnResult {
            text: t.text.trim().to_string(),
            stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sizes_and_uuid() {
        assert_eq!(parse_size("131.1K"), Some(131_100));
        assert_eq!(parse_size("1M"), Some(1_000_000));
        assert_eq!(parse_size("200000"), Some(200_000));
        let u = random_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
    }

    #[test]
    fn claude_stream_json() {
        let mut t = Turn::default();
        parse_line(
            Cli::Claude,
            &json!({"type":"system","subtype":"init","session_id":"s1"}),
            &mut t,
        );
        assert_eq!(t.session.as_deref(), Some("s1"));
        let ev = parse_line(
            Cli::Claude,
            &json!({"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Bash"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"output_tokens":3}}}),
            &mut t,
        );
        assert_eq!(ev.len(), 2);
        assert_eq!(t.used, Some(115));
        parse_line(
            Cli::Claude,
            &json!({"type":"result","subtype":"success","is_error":false,"result":"done","stop_reason":"end_turn","modelUsage":{"m":{"contextWindow":200000}}}),
            &mut t,
        );
        assert_eq!(
            (t.text.as_str(), t.stop.as_deref(), t.window),
            ("done", Some("end_turn"), Some(200000))
        );
        let mut e = Turn::default();
        parse_line(
            Cli::Claude,
            &json!({"type":"result","subtype":"error_during_execution","is_error":true,"result":""}),
            &mut e,
        );
        assert_eq!(e.error.as_deref(), Some("error_during_execution"));
    }

    #[test]
    fn codex_exec_json() {
        let mut t = Turn::default();
        parse_line(
            Cli::Codex,
            &json!({"type":"thread.started","thread_id":"th"}),
            &mut t,
        );
        let ev = parse_line(
            Cli::Codex,
            &json!({"type":"item.started","item":{"id":"i1","type":"command_execution","command":"ls"}}),
            &mut t,
        );
        assert!(
            matches!(&ev[0], DriverEvent::ToolCall { title, status, .. } if title == "ls" && status == "in_progress")
        );
        parse_line(
            Cli::Codex,
            &json!({"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"first"}}),
            &mut t,
        );
        parse_line(
            Cli::Codex,
            &json!({"type":"item.completed","item":{"id":"i3","type":"agent_message","text":"final"}}),
            &mut t,
        );
        parse_line(
            Cli::Codex,
            &json!({"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":5}}),
            &mut t,
        );
        assert_eq!(
            (
                t.session.as_deref(),
                t.text.as_str(),
                t.used,
                t.stop.as_deref()
            ),
            (Some("th"), "final", Some(1005), Some("end_turn"))
        );
        let mut e = Turn::default();
        parse_line(
            Cli::Codex,
            &json!({"type":"turn.failed","error":{"message":"usage limit"}}),
            &mut e,
        );
        assert_eq!(e.error.as_deref(), Some("usage limit"));
    }

    #[test]
    fn pi_json_mode() {
        let mut t = Turn::default();
        parse_line(Cli::Pi, &json!({"type":"session","id":"p1"}), &mut t);
        parse_line(
            Cli::Pi,
            &json!({"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"q"}]}}),
            &mut t,
        );
        parse_line(
            Cli::Pi,
            &json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"x"},{"type":"text","text":"\n\nOK"}],"usage":{"totalTokens":1606},"stopReason":"stop"}}),
            &mut t,
        );
        parse_line(
            Cli::Pi,
            &json!({"type":"agent_settled","aborted":false}),
            &mut t,
        );
        assert_eq!(
            (
                t.session.as_deref(),
                t.text.as_str(),
                t.used,
                t.stop.as_deref()
            ),
            (Some("p1"), "OK", Some(1606), Some("end_turn"))
        );
        let mut e = Turn::default();
        parse_line(
            Cli::Pi,
            &json!({"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"429 rate limited"}}),
            &mut e,
        );
        parse_line(
            Cli::Pi,
            &json!({"type":"agent_settled","aborted":false}),
            &mut e,
        );
        assert_eq!(
            (e.error.as_deref(), e.stop),
            (Some("429 rate limited"), None)
        );
    }

    #[test]
    fn args_follow_session_and_permissions() {
        let d = HeadlessDriver::new(Cli::Claude, "claude".into(), "/w".into(), vec![])
            .with_model(Some("sonnet".into()))
            .with_effort(Some("high".into()));
        *d.session.lock().unwrap() = Some("abc".into());
        d.fresh.store(true, Ordering::SeqCst);
        let a = d.args().join(" ");
        assert!(
            a.contains("--model sonnet") && a.contains("--session-id abc"),
            "{a}"
        );
        assert!(a.contains("--allowedTools Bash(aaf:*)"), "{a}");
        assert!(a.contains("--effort high"), "{a}");
        let x = HeadlessDriver::new(Cli::Codex, "codex".into(), "/w".into(), vec![])
            .with_effort(Some("xhigh".into()));
        assert!(x
            .args()
            .join(" ")
            .contains("-c model_reasoning_effort=xhigh"));
        let p = HeadlessDriver::new(Cli::Pi, "pi".into(), "/w".into(), vec![])
            .with_effort(Some("low".into()));
        assert!(p.args().join(" ").contains("--thinking low"));
        d.fresh.store(false, Ordering::SeqCst);
        assert!(d.args().join(" ").contains("--resume abc"));
        let y = HeadlessDriver::new(Cli::Codex, "codex".into(), "/w".into(), vec![])
            .with_mode(Some(crate::catalog::WIDEST.into()));
        *y.session.lock().unwrap() = Some("th".into());
        let a = y.args().join(" ");
        assert!(
            a.contains("--dangerously-bypass-approvals-and-sandbox"),
            "{a}"
        );
        assert!(a.ends_with("resume th -"), "{a}");
    }
}
