//! Driver 契約：新 agent 種類＝新 driver，dispatcher 不改。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Offline,
    Idle,
    Working,
    /// 等待人回應（ACP permission request）。
    Blocked,
}

/// 權限請求的一個選項（對應 ACP PermissionOption）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// allow_once／allow_always／reject_once／reject_always
    pub kind: String,
}

/// driver 對外的即時事件（工作中串流、權限卡片、狀態側欄的資料來源）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DriverEvent {
    Status {
        status: AgentStatus,
    },
    MessageChunk {
        text: String,
    },
    ThoughtChunk {
        text: String,
    },
    ToolCall {
        id: String,
        title: String,
        status: String,
    },
    /// Adapter-supplied evidence only; never replay as a tool invocation.
    ToolEvidence {
        data: serde_json::Value,
    },
    PermissionRequested {
        request_id: u64,
        title: String,
        options: Vec<PermissionOption>,
    },
    PermissionResolved {
        request_id: u64,
        decision: PermissionDecision,
    },
    TurnEnded {
        stop_reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionDecision {
    Selected { option_id: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnResult {
    pub text: String,
    /// end_turn／cancelled／max_tokens／refusal／error…
    pub stop_reason: String,
}

/// agent 回報的上下文用量（ACP `usage_update`）：目前在上下文裡的 token 數、上下文窗口大小。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextUsage {
    pub used: u64,
    pub size: u64,
    /// agent 自報名稱（ACP initialize 的 agentInfo.name），用來選換 session 的門檻
    pub agent: String,
}

/// 回合進行中插入新訊息（`Driver::steer`）的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerOutcome {
    /// driver 不支援插入
    Unsupported,
    /// 沒有進行中的回合（照一般流程等下一輪）
    NotRunning,
    /// 已插入進行中的回合
    Steered,
    /// agent 回報插入失敗
    Failed(String),
    /// 送達時回合剛好結束：agent 把它當成一般提示跑完一輪
    Turn(TurnResult),
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("agent not running")]
    NotRunning,
    #[error("agent busy")]
    Busy,
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("timeout")]
    Timeout,
}

pub type DResult<T> = Result<T, DriverError>;

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> &'static str;
    async fn start(&self) -> DResult<()>;
    /// 送一輪提示並等它結束；過程經 events() 串流。
    async fn prompt(&self, text: &str) -> DResult<TurnResult>;
    /// 取消當前回合（停止按鈕）。沒有進行中的回合時無作用。
    async fn cancel(&self) -> DResult<()>;
    /// 回合進行中插入新訊息（Hermes `/steer`）；預設不支援。
    async fn steer(&self, _text: &str) -> DResult<SteerOutcome> {
        Ok(SteerOutcome::Unsupported)
    }
    /// 回應權限請求（權限卡片按鈕）。
    async fn respond_permission(
        &self,
        request_id: u64,
        decision: PermissionDecision,
    ) -> DResult<()>;
    fn status(&self) -> AgentStatus;
    fn events(&self) -> broadcast::Receiver<DriverEvent>;
    async fn stop(&self) -> DResult<()>;
    /// 可續接的 session id（ACP）；其他 driver 為 None。dispatcher 存起來，重啟後續接。
    fn session_id(&self) -> Option<String> {
        None
    }
    /// Actual outcome of the latest start, not the requested resume id.
    fn session_loaded(&self) -> bool {
        false
    }
    /// 最近一次 agent 回報的上下文用量；不回報的 driver 為 None。
    fn context_usage(&self) -> Option<ContextUsage> {
        None
    }
    /// 最近一次回合附帶的額度視窗（Claude：5 小時／每週）；不回報的 driver 為 None。
    /// 格式見 `ratelimit::normalize`，`observed_at` 是收到的時間。
    fn rate_limit(&self) -> Option<serde_json::Value> {
        None
    }
    /// 尚未回應的權限請求（頁面載入時顯示權限卡片用）。
    fn pending_permissions(&self) -> Vec<DriverEvent> {
        Vec::new()
    }
}

/// roles 設定中的 driver 描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case")]
pub enum DriverSpec {
    Acp {
        /// 可省略：有 `acp_agent` 時由目錄解析
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        command: Vec<String>,
        /// 目錄中的 agent id（hermes／claude／codex／pi／自訂）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acp_agent: Option<String>,
        /// 模型 id；session 建立後切換
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// 權限模式 id（agent 回報的 modes／configOptions category=mode）；`@widest`＝最寬（YOLO）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        /// roles.json 寫成物件 {"KEY": "value"}；值支援 {role} {user} {home} {instance}
        #[serde(default, deserialize_with = "env_map")]
        env: Vec<(String, String)>,
        /// 帳號 id（`deploy/accounts.json`）；空＝預設登入。只有 claude／codex 有多帳號
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        /// 推理強度（agent 回報的 thought_level 選項，例 low／medium／high）；不支援的 agent 略過
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        /// `headless`＝不走 ACP 轉接器，每輪直接跑 agent 的非互動 CLI（只支援 claude／codex／pi，其他 agent 照常走 ACP）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transport: Option<String>,
    },
    Command {
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    Manual,
}

fn env_map<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<(String, String)>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum E {
        Map(std::collections::BTreeMap<String, String>),
        Pairs(Vec<(String, String)>),
    }
    Ok(match E::deserialize(d)? {
        E::Map(m) => m.into_iter().collect(),
        E::Pairs(p) => p,
    })
}

impl DriverSpec {
    pub fn is_manual(&self) -> bool {
        matches!(self, DriverSpec::Manual)
    }

    /// 設成 headless、而且 agent 有非互動 CLI 時回傳該 CLI；自訂 command 一律走 ACP。
    pub fn headless(&self) -> Option<crate::headless::Cli> {
        match self {
            DriverSpec::Acp {
                command,
                acp_agent: Some(a),
                transport: Some(t),
                ..
            } if t == "headless" && command.is_empty() => crate::headless::Cli::from_agent(a),
            _ => None,
        }
    }

    pub fn build(&self, default_cwd: &std::path::Path) -> Box<dyn Driver> {
        self.build_resuming(default_cwd, None, &LaunchContext::default())
            .expect("no env to validate")
    }

    /// 建立 driver；ACP 會在 start() 時嘗試以 session/load 續接 `resume`。env 會先驗證與代換。
    pub fn build_resuming(
        &self,
        default_cwd: &std::path::Path,
        resume: Option<String>,
        ctx: &LaunchContext,
    ) -> Result<Box<dyn Driver>, String> {
        Ok(match self {
            DriverSpec::Acp {
                command,
                acp_agent,
                model,
                mode,
                cwd,
                env,
                effort,
                account,
                transport,
            } => {
                let account_env = crate::accounts::env_for(
                    &ctx.instance,
                    acp_agent.as_deref(),
                    account.as_deref(),
                )?;
                if let Some(cli) = self.headless() {
                    let program = crate::catalog::which(cli.program())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| cli.program().into());
                    return Ok(Box::new(
                        crate::headless::HeadlessDriver::new(
                            cli,
                            program,
                            cwd.clone().unwrap_or_else(|| default_cwd.into()),
                            {
                                let mut e = account_env.clone();
                                e.extend(resolve_env(env, ctx)?);
                                e
                            },
                        )
                        .with_model(model.clone())
                        .with_mode(mode.clone())
                        .with_effort(effort.clone())
                        .with_account(account.clone())
                        .with_resume(resume)
                        .with_registry(ctx.agents.clone()),
                    ));
                }
                let _ = transport;
                let mut engine_env = vec![];
                let cmd = if command.is_empty() {
                    let a = acp_agent
                        .as_deref()
                        .ok_or("acp: command or acp_agent required")?;
                    // 轉接器用的本體＝aaf 看到、測過的那一個
                    engine_env = crate::catalog::engine_env(a, &ctx.instance);
                    crate::catalog::resolve_cmd(a, &ctx.instance)
                        .ok_or_else(|| crate::catalog::unavailable_reason(a, &ctx.instance))?
                } else {
                    command.clone()
                };
                Box::new(
                    crate::acp::AcpDriver::new(
                        cmd,
                        cwd.clone().unwrap_or_else(|| default_cwd.into()),
                        {
                            let mut e = engine_env;
                            e.extend(account_env);
                            e.extend(resolve_env(env, ctx)?);
                            e
                        },
                    )
                    .with_model(model.clone())
                    .with_mode(mode.clone())
                    .with_effort(effort.clone())
                    .with_account(account.clone())
                    .with_resume(resume)
                    .with_registry(ctx.agents.clone()),
                )
            }
            DriverSpec::Command { command, cwd } => Box::new(
                crate::command::CommandDriver::new(
                    command.clone(),
                    cwd.clone().unwrap_or_else(|| default_cwd.into()),
                )
                .with_env(resolve_env(&[], ctx)?)
                .with_registry(ctx.agents.clone()),
            ),
            DriverSpec::Manual => Box::new(crate::manual::ManualDriver::default()),
        })
    }
}

/// 角色程序的執行環境：env 樣板代換、agent 紀錄目錄。
#[derive(Debug, Clone, Default)]
pub struct LaunchContext {
    pub role: String,
    /// instance 根目錄（{instance}）
    pub instance: PathBuf,
    /// 記錄 agent 程序（啟動時清理殘留用）；None＝不記錄（測試）。
    pub agents: Option<crate::process::Registry>,
    /// 系統注入的環境變數（角色 token、API 位址）；最後套用，覆蓋 roles.json env。
    pub extra_env: Vec<(String, String)>,
}

/// 不可由 roles.json env 覆寫的變數（身分與執行路徑）。
pub const PROTECTED_ENV: &[&str] = &[
    "AAF_ROLE",
    "AAF_TOKEN",
    "AAF_HOME",
    "MBOX_AGENT",
    "MBOX_TOKEN",
    "MBOX_HOME",
    "PATH",
];

/// 代換 {role} {user} {home} {instance}，只代換一次（插入的值含大括號也不會再展開）。
pub fn expand_template(v: &str, ctx: &LaunchContext) -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let (rep, len): (Option<String>, usize) = if tail.starts_with("{role}") {
            (Some(ctx.role.clone()), 6)
        } else if tail.starts_with("{user}") {
            (Some(user.clone()), 6)
        } else if tail.starts_with("{home}") {
            (Some(home.clone()), 6)
        } else if tail.starts_with("{instance}") {
            (Some(ctx.instance.display().to_string()), 10)
        } else {
            (None, 1)
        };
        match rep {
            Some(r) => out.push_str(&r),
            None => out.push('{'),
        }
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// 檢查並展開角色 env：碰到受保護變數回錯誤。
pub fn resolve_env(
    env: &[(String, String)],
    ctx: &LaunchContext,
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (k, v) in env {
        if PROTECTED_ENV.contains(&k.as_str()) {
            return Err(format!("env {k} is protected and cannot be overridden"));
        }
        out.push((k.clone(), expand_template(v, ctx)));
    }
    let id = [
        ("AAF_ROLE".to_string(), ctx.role.clone()),
        ("AAF_HOME".to_string(), ctx.instance.display().to_string()),
    ];
    out.extend(id);
    out.extend(ctx.extra_env.iter().cloned());
    Ok(out)
}

/// 共用：狀態＋事件廣播。
pub(crate) struct Signals {
    pub status: std::sync::Mutex<AgentStatus>,
    pub tx: broadcast::Sender<DriverEvent>,
}

impl Signals {
    pub fn new(initial: AgentStatus) -> Self {
        Self {
            status: std::sync::Mutex::new(initial),
            // 夠大才不會在 session 載入回放歷史這類爆量時擠掉事件（帳本少事件會把 lane 擋住）
            tx: broadcast::channel(16384).0,
        }
    }
    pub fn set(&self, s: AgentStatus) {
        let mut cur = self.status.lock().unwrap();
        if *cur != s {
            *cur = s;
            let _ = self.tx.send(DriverEvent::Status { status: s });
        }
    }
    pub fn get(&self) -> AgentStatus {
        *self.status.lock().unwrap()
    }
    pub fn emit(&self, e: DriverEvent) {
        let _ = self.tx.send(e);
    }
}
