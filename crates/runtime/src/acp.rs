//! ACP v1 client（stdio JSON-RPC 2.0）：每個角色一個常駐 agent 子程序。
//!
//! 只實作 client 端需要的部分：initialize、session/new、session/prompt、session/cancel、
//! session/update（串流）、session/request_permission（轉成權限卡片，等人回應）。
//! 不宣告 fs／terminal 能力，agent 的檔案與指令操作由 agent 自己在 cwd 內處理。

use crate::driver::*;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, oneshot, Mutex};

pub const PROTOCOL_VERSION: u64 = 1;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>;

struct Conn {
    stdin: Mutex<tokio::process::ChildStdin>,
    /// 程序群組 id（＝leader pid）
    pid: Option<u32>,
    child: Mutex<tokio::process::Child>,
    pending: Pending,
    next_id: AtomicU64,
    session_id: std::sync::Mutex<String>,
}

impl Conn {
    fn sid(&self) -> String {
        self.session_id.lock().unwrap().clone()
    }
}

struct Shared {
    sig: Signals,
    /// 權限請求：我方 request_id → (agent 的 JSON-RPC id)
    permissions: Mutex<HashMap<u64, Value>>,
    /// 待回應權限請求的內容（與 permissions 同步增刪）
    perm_events: std::sync::Mutex<HashMap<u64, DriverEvent>>,
    next_perm: AtomicU64,
    turn_text: Mutex<String>,
    replaying: std::sync::atomic::AtomicBool,
    /// 是否有 session/prompt 進行中。回合外（例如背景子任務）的權限核准後要回 Idle，不能變成 Working。
    in_turn: std::sync::atomic::AtomicBool,
    /// 最近一次 usage_update 的 (used, size)；換 session 時清掉
    usage: std::sync::Mutex<Option<(u64, u64)>>,
    /// 最近一次 usage_update 附帶的額度資訊（Claude 轉接器的 `_claude/rateLimit`）和收到的時間
    rate: std::sync::Mutex<Option<(f64, Value)>>,
    /// initialize 回報的 agentInfo.name
    agent_name: std::sync::Mutex<String>,
}

pub struct AcpDriver {
    command: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    shared: Arc<Shared>,
    conn: Mutex<Option<Arc<Conn>>>,
    /// 上次的 session id；start() 時若 agent 支援 loadSession 就續接。
    resume: std::sync::Mutex<Option<String>>,
    loaded: std::sync::atomic::AtomicBool,
    registry: Option<crate::process::Registry>,
    /// 想要的模型；session 建立／續接後依 agent 回報切換
    model: Option<String>,
    /// 想要的權限模式；`@widest`＝agent 回報清單裡最寬的那個
    mode: Option<String>,
    /// 想要的推理強度（thought_level 選項）
    effort: Option<String>,
    /// 用的帳號 id（只用來標記額度資訊屬於哪個帳號）
    account: Option<String>,
    pub request_timeout: Duration,
}

impl AcpDriver {
    /// 記錄 agent 程序，讓 aaf 異常結束後下次啟動能清理殘留。
    pub fn with_registry(mut self, r: Option<crate::process::Registry>) -> Self {
        self.registry = r;
        self
    }

    pub fn with_model(mut self, m: Option<String>) -> Self {
        self.model = m;
        self
    }

    pub fn with_effort(mut self, e: Option<String>) -> Self {
        self.effort = e.filter(|e| !e.is_empty());
        self
    }

    pub fn with_mode(mut self, m: Option<String>) -> Self {
        self.mode = m;
        self
    }

    pub fn with_account(mut self, a: Option<String>) -> Self {
        self.account = a.filter(|a| !a.is_empty());
        self
    }

    /// 指定要續接的 session（例如從資料庫讀回）。
    pub fn with_resume(self, session_id: Option<String>) -> Self {
        *self.resume.lock().unwrap() = session_id;
        self
    }

    pub fn new(command: Vec<String>, cwd: PathBuf, env: Vec<(String, String)>) -> Self {
        Self {
            command,
            cwd,
            env,
            shared: Arc::new(Shared {
                sig: Signals::new(AgentStatus::Offline),
                permissions: Mutex::new(HashMap::new()),
                perm_events: Default::default(),
                next_perm: AtomicU64::new(1),
                turn_text: Mutex::new(String::new()),
                replaying: Default::default(),
                in_turn: Default::default(),
                usage: Default::default(),
                rate: Default::default(),
                agent_name: Default::default(),
            }),
            conn: Mutex::new(None),
            loaded: Default::default(),
            resume: std::sync::Mutex::new(None),
            registry: None,
            model: None,
            mode: None,
            effort: None,
            account: None,
            request_timeout: Duration::from_secs(120),
        }
    }

    async fn conn(&self) -> DResult<Arc<Conn>> {
        self.conn
            .lock()
            .await
            .clone()
            .ok_or(DriverError::NotRunning)
    }
}

impl Conn {
    async fn send(&self, msg: Value) -> DResult<()> {
        let mut line =
            serde_json::to_vec(&msg).map_err(|e| DriverError::Protocol(e.to_string()))?;
        line.push(b'\n');
        let mut w = self.stdin.lock().await;
        w.write_all(&line).await?;
        w.flush().await?;
        Ok(())
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> DResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let r = match timeout {
            Some(t) => tokio::time::timeout(t, rx)
                .await
                .map_err(|_| DriverError::Timeout)?,
            None => rx.await,
        };
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(DriverError::Protocol(format!("{method}: {e}"))),
            Err(_) => Err(DriverError::NotRunning),
        }
    }
}

fn text_of(content: &Value) -> Option<&str> {
    (content["type"] == "text")
        .then(|| content["text"].as_str())
        .flatten()
}

async fn handle_incoming(
    msg: Value,
    pending: &Pending,
    shared: &Arc<Shared>,
    stdin_conn: &Arc<Mutex<Option<Arc<Conn>>>>,
) {
    let method = msg.get("method").and_then(Value::as_str);
    match (method, msg.get("id")) {
        // 回應我們的請求
        (None, Some(id)) => {
            if let Some(id) = id.as_u64() {
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let _ = tx.send(match msg.get("error") {
                        Some(e) => Err(e.clone()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    });
                }
            }
        }
        // 通知
        (Some("session/update"), None) => {
            if shared.replaying.load(Ordering::SeqCst) {
                return;
            }
            let u = &msg["params"]["update"];
            match u["sessionUpdate"].as_str() {
                Some("agent_message_chunk") => {
                    if let Some(t) = text_of(&u["content"]) {
                        shared.turn_text.lock().await.push_str(t);
                        shared
                            .sig
                            .emit(DriverEvent::MessageChunk { text: t.into() });
                    }
                }
                Some("agent_thought_chunk") => {
                    if let Some(t) = text_of(&u["content"]) {
                        shared
                            .sig
                            .emit(DriverEvent::ThoughtChunk { text: t.into() });
                    }
                }
                Some("usage_update") => {
                    if let (Some(used), Some(size)) = (u["used"].as_u64(), u["size"].as_u64()) {
                        *shared.usage.lock().unwrap() = Some((used, size));
                    }
                    if u["_meta"]["_claude/rateLimit"].is_object() {
                        *shared.rate.lock().unwrap() = Some((
                            crate::ratelimit::now_secs(),
                            u["_meta"]["_claude/rateLimit"].clone(),
                        ));
                    }
                }
                Some("tool_call") | Some("tool_call_update") => {
                    shared.sig.emit(DriverEvent::ToolCall {
                        id: u["toolCallId"].as_str().unwrap_or_default().into(),
                        title: u["title"].as_str().unwrap_or_default().into(),
                        status: u["status"].as_str().unwrap_or_default().into(),
                    });
                    shared.sig.emit(DriverEvent::ToolEvidence { data: json!({
                        "id":u["toolCallId"],"title":u["title"],"status":u["status"],
                        "input":u.get("rawInput"),"output":u.get("rawOutput"),
                        "content":u.get("content"),"locations":u.get("locations"),
                        "coverage": if u.get("rawOutput").is_some() || u.get("content").is_some() { "adapter supplied; external effects unverified" } else { "coverage gap: tool result not supplied; effects unknown" }
                    }) });
                }
                _ => {}
            }
        }
        // agent 對我們的請求
        (Some(m), Some(id)) => {
            let conn = stdin_conn.lock().await.clone();
            if m == "session/request_permission" {
                let rid = shared.next_perm.fetch_add(1, Ordering::SeqCst);
                shared.permissions.lock().await.insert(rid, id.clone());
                let p = &msg["params"];
                let options = p["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| PermissionOption {
                        id: o["optionId"].as_str().unwrap_or_default().into(),
                        name: o["name"].as_str().unwrap_or_default().into(),
                        kind: o["kind"].as_str().unwrap_or_default().into(),
                    })
                    .collect();
                shared.sig.set(AgentStatus::Blocked);
                let ev = DriverEvent::PermissionRequested {
                    request_id: rid,
                    title: p["toolCall"]["title"].as_str().unwrap_or_default().into(),
                    options,
                };
                shared.perm_events.lock().unwrap().insert(rid, ev.clone());
                shared.sig.emit(ev);
            } else if let Some(c) = conn {
                let _ = c
                    .send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not supported by client: {m}")}}))
                    .await;
            }
        }
        _ => {}
    }
}

#[async_trait]
impl Driver for AcpDriver {
    fn kind(&self) -> &'static str {
        "acp"
    }

    async fn start(&self) -> DResult<()> {
        if self.conn.lock().await.is_some() {
            if self.status() != AgentStatus::Offline {
                return Ok(());
            }
            // The reader marks a dead child Offline but retains its connection.
            // Reap it before spawning; keep the session id for session/load.
            self.stop().await?;
        }
        let (prog, args) = self
            .command
            .split_first()
            .ok_or_else(|| DriverError::Protocol("empty command".into()))?;
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args)
            .current_dir(&self.cwd)
            .envs(self.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        crate::process::isolate(&mut cmd);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        if let (Some(r), Some(pid)) = (&self.registry, pid) {
            let _ = r.record(pid, &self.command.join(" "));
        }
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let pending: Pending = Default::default();
        let slot: Arc<Mutex<Option<Arc<Conn>>>> = Arc::new(Mutex::new(None));
        {
            let (pending, shared, slot) = (pending.clone(), self.shared.clone(), slot.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        handle_incoming(v, &pending, &shared, &slot).await;
                    }
                }
                // agent 結束：所有等待中的請求失敗，狀態轉離線
                pending.lock().await.clear();
                shared.sig.set(AgentStatus::Offline);
            });
        }
        let boot = Arc::new(Conn {
            stdin: Mutex::new(stdin),
            pid,
            child: Mutex::new(child),
            pending,
            next_id: AtomicU64::new(1),
            session_id: Default::default(),
        });
        *slot.lock().await = Some(boot.clone());
        let t = Some(self.request_timeout);
        let init = boot
            .request(
                "initialize",
                json!({"protocolVersion": PROTOCOL_VERSION,
                       "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                       "clientInfo": {"name": "aaf", "version": env!("CARGO_PKG_VERSION")}}),
                t,
            )
            .await?;
        if init["protocolVersion"].as_u64() != Some(PROTOCOL_VERSION) {
            return Err(DriverError::Protocol(format!(
                "unsupported protocol version: {}",
                init["protocolVersion"]
            )));
        }
        *self.shared.agent_name.lock().unwrap() = init["agentInfo"]["name"]
            .as_str()
            .unwrap_or_default()
            .into();
        *self.shared.usage.lock().unwrap() = None;
        let cwd = std::fs::canonicalize(&self.cwd)?;
        let can_load = init["agentCapabilities"]["loadSession"].as_bool() == Some(true);
        let resume = self.resume.lock().unwrap().clone();
        let mut session_id = None;
        if let (Some(sid), true) = (resume, can_load) {
            // 續接：agent 會以 session/update 重播歷史，不計入本輪回覆
            self.shared.replaying.store(true, Ordering::SeqCst);
            let r = boot
                .request(
                    "session/load",
                    json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
                    t,
                )
                .await;
            self.shared.replaying.store(false, Ordering::SeqCst);
            if r.is_ok() {
                session_id = Some(sid);
            } else {
                tracing::warn!("session/load failed, starting new session");
            }
        }
        let loaded = session_id.is_some();
        let (session_id, mut session_info) = match session_id {
            Some(s) => (s, Value::Null),
            None => {
                let r = boot
                    .request("session/new", json!({"cwd": cwd, "mcpServers": []}), t)
                    .await?;
                let sid = r["sessionId"]
                    .as_str()
                    .ok_or_else(|| DriverError::Protocol("session/new: no sessionId".into()))?
                    .to_string();
                (sid, r)
            }
        };
        if let Some(want) = &self.model {
            // 續接的 session 沒有回報清單：直接用 set_model（不支援就略過，模型由 agent 保留）
            let sw = if session_info.is_null() {
                Ok(Some((
                    "session/set_model".to_string(),
                    json!({"modelId": want}),
                    want.clone(),
                )))
            } else {
                crate::catalog::model_switch(&session_info, want)
            };
            match sw {
                Err(e) => {
                    let _ = self.stop().await;
                    return Err(DriverError::Protocol(e));
                }
                Ok(Some((method, mut params, _))) => {
                    params["sessionId"] = json!(session_id);
                    let r = boot.request(&method, params, t).await;
                    if let (Err(e), false) = (&r, session_info.is_null()) {
                        let msg = format!("{method}: {e}");
                        let _ = self.stop().await;
                        return Err(DriverError::Protocol(msg));
                    }
                    // 推理強度的選項可能隨模型不同：用切換後回報的設定
                    if let Ok(v) = &r {
                        if v["configOptions"].is_array() && !session_info.is_null() {
                            session_info["configOptions"] = v["configOptions"].clone();
                        }
                    }
                }
                Ok(None) => {}
            }
        }
        if let Some(want) = &self.mode {
            // 權限模式：新 session 依回報清單選；續接的 session 沒有清單，依序試（不支援就略過，不擋回合）
            let tries = if session_info.is_null() {
                crate::catalog::mode_candidates(want)
                    .into_iter()
                    .map(|m| ("session/set_mode".to_string(), json!({"modeId": m}), m))
                    .collect()
            } else {
                crate::catalog::mode_switch(&session_info, want)
                    .into_iter()
                    .collect::<Vec<_>>()
            };
            let mut applied = None;
            for (method, mut params, id) in tries {
                params["sessionId"] = json!(session_id);
                if boot.request(&method, params, t).await.is_ok() {
                    applied = Some(id);
                    break;
                }
            }
            tracing::info!(want = %want, applied = ?applied, "permission mode");
        }
        if let Some(want) = &self.effort {
            // 推理強度：新 session 依回報選項切；續接的 session 沒有清單，依常見設定 id 試。不支援就略過，不擋回合
            let tries: Vec<(String, Value)> = if session_info.is_null() {
                ["effort", "reasoning_effort", "thought_level"]
                    .iter()
                    .map(|id| {
                        (
                            "session/set_config_option".to_string(),
                            json!({"configId": id, "value": want}),
                        )
                    })
                    .collect()
            } else {
                match crate::catalog::effort_switch(&session_info, want) {
                    Ok(s) => s.into_iter().collect(),
                    Err(e) => {
                        tracing::warn!(want = %want, error = %e, "reasoning effort not applied");
                        vec![]
                    }
                }
            };
            let mut applied = false;
            for (method, mut params) in tries {
                params["sessionId"] = json!(session_id);
                if boot.request(&method, params, t).await.is_ok() {
                    applied = true;
                    break;
                }
            }
            tracing::info!(want = %want, applied, "reasoning effort");
        }
        *self.resume.lock().unwrap() = Some(session_id.clone());
        self.loaded.store(loaded, Ordering::SeqCst);
        *boot.session_id.lock().unwrap() = session_id;
        *self.conn.lock().await = Some(boot);
        self.shared.sig.set(AgentStatus::Idle);
        Ok(())
    }

    async fn prompt(&self, text: &str) -> DResult<TurnResult> {
        let conn = self.conn().await?;
        if self.shared.sig.get() != AgentStatus::Idle {
            return Err(DriverError::Busy);
        }
        self.shared.turn_text.lock().await.clear();
        self.shared.sig.set(AgentStatus::Working);
        self.shared.in_turn.store(true, Ordering::SeqCst);
        let r = conn
            .request(
                "session/prompt",
                json!({"sessionId": conn.sid(), "prompt": [{"type": "text", "text": text}]}),
                None,
            )
            .await;
        self.shared.in_turn.store(false, Ordering::SeqCst);
        // 回合結束：未回應的權限請求一律取消
        for (rid, id) in self.shared.permissions.lock().await.drain() {
            let _ = conn.send(json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}})).await;
            self.shared.perm_events.lock().unwrap().remove(&rid);
            self.shared.sig.emit(DriverEvent::PermissionResolved {
                request_id: rid,
                decision: PermissionDecision::Cancelled,
            });
        }
        if self.shared.sig.get() != AgentStatus::Offline {
            self.shared.sig.set(AgentStatus::Idle);
        }
        let v = r?;
        let stop_reason = v["stopReason"].as_str().unwrap_or("unknown").to_string();
        self.shared.sig.emit(DriverEvent::TurnEnded {
            stop_reason: stop_reason.clone(),
        });
        Ok(TurnResult {
            text: self.shared.turn_text.lock().await.clone(),
            stop_reason,
        })
    }

    async fn steer(&self, text: &str) -> DResult<SteerOutcome> {
        let conn = self.conn().await?;
        if !self.shared.in_turn.load(Ordering::SeqCst) {
            return Ok(SteerOutcome::NotRunning);
        }
        let body = text.trim();
        let v = conn
            .request(
                "session/prompt",
                json!({"sessionId": conn.sid(), "prompt": [{"type": "text", "text": format!("/steer {body}")}]}),
                None,
            )
            .await?;
        // Hermes 以一則 agent 訊息回報插入結果（acp_adapter/commands.py `_cmd_steer`），
        // 它會混進進行中回合的文字：認出來後從回合文字移除，不能變成角色回覆的一部分。
        let preview: String = if body.chars().count() > 80 {
            format!("{}...", body.chars().take(80).collect::<String>())
        } else {
            body.to_string()
        };
        let ack = format!("⏩ Steer queued for the active turn: {preview}");
        let mut t = self.shared.turn_text.lock().await;
        if let Some(p) = t.find(&ack) {
            t.replace_range(p..p + ack.len(), "");
            return Ok(SteerOutcome::Steered);
        }
        if let Some(p) = t.find("⚠️ Steer failed") {
            let end = t[p..].find('\n').map(|e| p + e).unwrap_or(t.len());
            let msg = t[p..end].to_string();
            t.replace_range(p..end, "");
            return Ok(SteerOutcome::Failed(msg));
        }
        if self.shared.in_turn.load(Ordering::SeqCst) {
            // 回合還在跑卻沒看到回條：不猜，留在佇列照一般流程
            return Ok(SteerOutcome::Failed("no steer acknowledgement".into()));
        }
        // 送到時回合已結束：Hermes 把 idle 的 /steer 當一般提示跑完一輪，回覆就是這輪的文字
        Ok(SteerOutcome::Turn(TurnResult {
            text: std::mem::take(&mut *t),
            stop_reason: v["stopReason"].as_str().unwrap_or("unknown").to_string(),
        }))
    }

    async fn cancel(&self) -> DResult<()> {
        let conn = self.conn().await?;
        // ACP：取消時 client 須以 cancelled 回應所有待決的權限請求
        for (rid, id) in self.shared.permissions.lock().await.drain() {
            conn.send(json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}})).await?;
            self.shared.perm_events.lock().unwrap().remove(&rid);
            self.shared.sig.emit(DriverEvent::PermissionResolved {
                request_id: rid,
                decision: PermissionDecision::Cancelled,
            });
        }
        conn.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": conn.sid()}})).await
    }

    async fn respond_permission(
        &self,
        request_id: u64,
        decision: PermissionDecision,
    ) -> DResult<()> {
        let conn = self.conn().await?;
        let id = self
            .shared
            .permissions
            .lock()
            .await
            .remove(&request_id)
            .ok_or_else(|| DriverError::Protocol(format!("no pending permission {request_id}")))?;
        let outcome = match &decision {
            PermissionDecision::Selected { option_id } => {
                json!({"outcome": "selected", "optionId": option_id})
            }
            PermissionDecision::Cancelled => json!({"outcome": "cancelled"}),
        };
        conn.send(json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}}))
            .await?;
        if self.shared.permissions.lock().await.is_empty()
            && self.shared.sig.get() == AgentStatus::Blocked
        {
            let in_turn = self.shared.in_turn.load(Ordering::SeqCst);
            self.shared.sig.set(if in_turn {
                AgentStatus::Working
            } else {
                AgentStatus::Idle
            });
        }
        self.shared.perm_events.lock().unwrap().remove(&request_id);
        self.shared.sig.emit(DriverEvent::PermissionResolved {
            request_id,
            decision,
        });
        Ok(())
    }

    fn status(&self) -> AgentStatus {
        self.shared.sig.get()
    }

    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.shared.sig.tx.subscribe()
    }

    fn session_id(&self) -> Option<String> {
        self.resume.lock().unwrap().clone()
    }

    fn session_loaded(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    fn context_usage(&self) -> Option<crate::driver::ContextUsage> {
        let (used, size) = (*self.shared.usage.lock().unwrap())?;
        Some(crate::driver::ContextUsage {
            used,
            size,
            agent: self.shared.agent_name.lock().unwrap().clone(),
        })
    }

    fn rate_limit(&self) -> Option<Value> {
        let (at, info) = self.shared.rate.lock().unwrap().clone()?;
        crate::ratelimit::normalize(&info, self.account.as_deref().unwrap_or("default"), at)
    }

    fn pending_permissions(&self) -> Vec<DriverEvent> {
        let mut v: Vec<_> = self
            .shared
            .perm_events
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|e| match e {
            DriverEvent::PermissionRequested { request_id, .. } => *request_id,
            _ => 0,
        });
        v
    }

    async fn stop(&self) -> DResult<()> {
        let mut owner = self.conn.lock().await;
        if let Some(c) = owner.as_ref() {
            // This lock serializes every group signal and the sole reap. Keep conn and
            // registry on *any* unknown result, including EPERM and wait errors.
            let outcome =
                crate::process::stop_owned(&mut *c.child.lock().await, Duration::from_secs(3))
                    .await;
            if outcome == crate::process::ReapOutcome::Unverified {
                return Err(DriverError::Protocol(
                    "owned process-group stop unverified; connection and registry retained".into(),
                ));
            }
            if let (Some(r), Some(pid)) = (&self.registry, c.pid) {
                r.forget(pid);
            }
            *owner = None;
        }
        self.shared.sig.set(AgentStatus::Offline);
        Ok(())
    }
}
