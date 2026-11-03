//! dispatcher：以「工作線（lane）＝角色＋群」為排程單位。
//!
//! - 每條 lane 有自己的 driver 與 ACP session：不同群可並行，同一 lane 內依序排隊。
//! - 不屬於任何群的信走該角色的預設 lane（room = None）。
//! - 回覆依原信的群與討論串寄回（[`Inbox::reply`]），不是只回最後一位寄件者。
//! - 閒置超過 `idle_park` 的 lane 收起（停 driver、保留 session id），下次有信時以 session/load 續接。
//! - 每群可覆寫 agent／模型／權限模式，並可開 YOLO：[`RoomOverride`]、[`Roster::spec_for`]。
//!
//! 信箱以 [`Inbox`] trait 抽象（實作在 chat feature）；dispatcher 不認識任何 agent 種類。

use crate::driver::{AgentStatus, Driver, DriverEvent, DriverSpec};
use aaf_core::events::EventBus;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub type RoomId = i64;

/// 工作線：角色＋群（None＝預設 lane）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct Lane {
    pub role: String,
    pub room: Option<RoomId>,
}

impl Lane {
    pub fn new(role: &str, room: Option<RoomId>) -> Self {
        Self {
            role: role.into(),
            room,
        }
    }
}

impl std::fmt::Display for Lane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.room {
            Some(r) => write!(f, "{}@{}", self.role, r),
            None => write!(f, "{}", self.role),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    pub id: i64,
    pub from: String,
    pub body: String,
    pub room: Option<RoomId>,
    pub thread_id: Option<i64>,
}

#[async_trait]
pub trait Inbox: Send + Sync {
    /// 有未讀信的 lane（已依角色是否開每群 lane 正規化）。
    async fn lanes_with_unread(&self) -> aaf_core::Result<Vec<Lane>>;
    /// 某 lane 的未讀（只回該群的信；預設 lane 只回不屬於任何群的信，或角色不開每群 lane 時全部）。
    async fn unread(&self, lane: &Lane) -> aaf_core::Result<Vec<InboxMessage>>;
    /// 回合成功後標記已處理（失敗不標，下輪重送：at-least-once）。
    async fn ack(&self, role: &str, ids: &[i64]) -> aaf_core::Result<()>;
    /// 以 `role` 身分回覆 `to`：沿用原信的群與討論串。
    async fn reply(&self, role: &str, to: &InboxMessage, body: &str) -> aaf_core::Result<()>;
    /// 平台系統公告（不以角色身分、不經角色發文規則）。回傳 Some(訊息 id)＝確實寫入；
    /// None＝這個信箱不支援或沒寫進去（群已封存等）。
    async fn notify(&self, _room: Option<RoomId>, _body: &str) -> aaf_core::Result<Option<i64>> {
        Ok(None)
    }
    /// 存取 lane 的 session id（續接用）；空字串＝清除。
    async fn load_session(&self, lane: &Lane) -> aaf_core::Result<Option<String>>;
    async fn save_session(&self, lane: &Lane, session_id: &str) -> aaf_core::Result<()>;
    /// 遺忘某角色所有已存的 session（設定變更後不可續接舊 agent 的 session）。
    async fn forget_sessions(&self, _role: &str) -> aaf_core::Result<()> {
        Ok(())
    }
}

/// 角色設定：預設 driver＋每群覆寫。
#[derive(Debug, Clone, Default)]
pub struct Roster {
    pub roles: HashMap<String, RoleSpec>,
    /// instance 根目錄（env 樣板 {instance}）
    pub instance: PathBuf,
    /// agent 程序紀錄（啟動時清理殘留）；None＝不記錄
    pub agents: Option<crate::process::Registry>,
    /// roles.json 路徑（熱重載用）；None＝不監看
    pub source: Option<PathBuf>,
}

/// dispatcher、信箱、HTTP 共用的角色設定；熱重載時整份換掉。
pub type SharedRoster = Arc<std::sync::RwLock<Arc<Roster>>>;

pub fn shared_roster(r: Roster) -> SharedRoster {
    Arc::new(std::sync::RwLock::new(Arc::new(r)))
}

/// 熱重載的差異。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RosterDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    /// 只有某些群的覆寫變了：只重開這些 lane（角色其他群不動）
    pub lanes: Vec<Lane>,
}

impl RosterDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.lanes.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleSpec {
    pub driver: DriverSpec,
    /// lead／worker：決定角色 API 權限
    pub rank: String,
    pub workdir: PathBuf,
    /// 是否每群一條 lane；false 時所有信都走預設 lane。
    pub room_lanes: bool,
    /// 每群覆寫（每群切換引擎、模型、權限模式、YOLO）。
    pub room_overrides: HashMap<RoomId, RoomOverride>,
}

/// 每群覆寫：疊加在角色全域設定上，只寫有變的欄位；`full` 是舊格式（整份 driver 取代）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomOverride {
    pub full: Option<DriverSpec>,
    pub acp_agent: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
    /// 推理強度（agent 回報的 thought_level 選項）
    pub effort: Option<String>,
    /// 本群用的帳號（accounts.json 的 id；default＝預設登入）
    pub account: Option<String>,
    /// YOLO：agent 切最寬權限模式＋本群所有權限請求自動允許
    pub yolo: bool,
    /// YOLO 期限（YYYY-MM-DD，含當日）；None＝不限
    pub yolo_until: Option<String>,
}

impl RoomOverride {
    /// 今天（YYYY-MM-DD）YOLO 是否生效。
    pub fn yolo_on(&self, today: &str) -> bool {
        self.yolo && self.yolo_until.as_deref().is_none_or(|u| today <= u)
    }

    /// 疊加到全域 driver 上，得到本群實際用的 driver。
    pub fn apply(&self, base: &DriverSpec, today: &str) -> DriverSpec {
        let mut d = self.full.clone().unwrap_or_else(|| base.clone());
        if let DriverSpec::Acp {
            command,
            acp_agent,
            model,
            mode,
            effort,
            account,
            ..
        } = &mut d
        {
            if let Some(a) = &self.acp_agent {
                if acp_agent.as_deref() != Some(a) {
                    command.clear();
                    *model = None;
                    *mode = None;
                    *effort = None;
                    *account = None;
                }
                *acp_agent = Some(a.clone());
            }
            if let Some(m) = &self.model {
                *model = Some(m.clone());
            }
            if let Some(m) = &self.mode {
                *mode = Some(m.clone());
            }
            if let Some(e) = &self.effort {
                *effort = Some(e.clone());
            }
            if let Some(a) = &self.account {
                *account = Some(a.clone());
            }
            if self.yolo_on(today) {
                *mode = Some(crate::catalog::WIDEST.into());
            }
        }
        d
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum OverrideFile {
    Full(DriverSpec),
    Layer {
        #[serde(default)]
        acp_agent: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        effort: Option<String>,
        #[serde(default)]
        account: Option<String>,
        #[serde(default)]
        yolo: bool,
        #[serde(default)]
        yolo_until: Option<String>,
    },
}

/// 本機今天（YYYY-MM-DD；依 TZ 環境變數，預設系統時區）。
pub fn today() -> String {
    jiff::Zoned::now().strftime("%Y-%m-%d").to_string()
}

impl RoleSpec {
    pub fn new(driver: DriverSpec, workdir: impl Into<PathBuf>) -> Self {
        Self {
            driver,
            workdir: workdir.into(),
            rank: "worker".into(),
            room_lanes: true,
            room_overrides: HashMap::new(),
        }
    }
}

/// roles.json 的格式：
/// `{"roles": {"coder": {"driver": "acp", "command": [...], "room_lanes": true, "workdir": "work/coder",
///                      "room_overrides": {"9": {"driver": "command", "command": [...]}}}}}`
#[derive(Debug, serde::Deserialize)]
struct RosterFile {
    roles: HashMap<String, RoleFile>,
}

#[derive(Debug, serde::Deserialize)]
struct RoleFile {
    #[serde(flatten)]
    driver: DriverSpec,
    #[serde(default = "worker")]
    rank: String,
    #[serde(default)]
    workdir: Option<PathBuf>,
    #[serde(default = "yes")]
    room_lanes: bool,
    #[serde(default)]
    room_overrides: HashMap<String, OverrideFile>,
}

fn yes() -> bool {
    true
}

fn worker() -> String {
    "worker".into()
}

/// 角色名稱：小寫英數、- 與 _，1–32 字。
pub fn valid_role_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl Roster {
    /// 解析 roles.json；相對 workdir 以 `home` 為基準，未指定時為 `home/work/<role>`（自動建立）。
    pub fn from_json(text: &str, home: &std::path::Path) -> aaf_core::Result<Self> {
        let f: RosterFile = serde_json::from_str(text)
            .map_err(|e| aaf_core::Error::Config(format!("roles.json: {e}")))?;
        let mut roles = HashMap::new();
        for (name, r) in f.roles {
            if !valid_role_name(&name) {
                return Err(aaf_core::Error::Config(format!(
                    "roles.json: invalid role name {name:?}"
                )));
            }
            let workdir = match r.workdir {
                Some(p) if p.is_absolute() => p,
                Some(p) => home.join(p),
                None => home.join("work").join(&name),
            };
            std::fs::create_dir_all(&workdir)
                .map_err(|e| aaf_core::Error::Config(format!("{}: {e}", workdir.display())))?;
            let mut overrides = HashMap::new();
            for (room, spec) in r.room_overrides {
                let room: RoomId = room.parse().map_err(|_| {
                    aaf_core::Error::Config(format!("roles.json: {name}: room id {room:?}"))
                })?;
                let o = match spec {
                    OverrideFile::Full(d) => RoomOverride {
                        full: Some(d),
                        ..Default::default()
                    },
                    OverrideFile::Layer {
                        acp_agent,
                        model,
                        mode,
                        effort,
                        account,
                        yolo,
                        yolo_until,
                    } => {
                        if let Some(u) = &yolo_until {
                            if jiff::civil::Date::strptime("%Y-%m-%d", u).is_err() {
                                return Err(aaf_core::Error::Config(format!(
                                    "roles.json: {name}: room {room}: yolo_until must be YYYY-MM-DD"
                                )));
                            }
                        }
                        RoomOverride {
                            full: None,
                            acp_agent,
                            model,
                            mode,
                            effort,
                            account,
                            yolo,
                            yolo_until,
                        }
                    }
                };
                overrides.insert(room, o);
            }
            for spec in
                std::iter::once(&r.driver).chain(overrides.values().filter_map(|o| o.full.as_ref()))
            {
                if let DriverSpec::Acp { env, .. } = spec {
                    if let Some((k, _)) = env
                        .iter()
                        .find(|(k, _)| crate::driver::PROTECTED_ENV.contains(&k.as_str()))
                    {
                        return Err(aaf_core::Error::Config(format!(
                            "roles.json: {name}: env {k} is protected"
                        )));
                    }
                }
            }
            if r.rank != "lead" && r.rank != "worker" {
                return Err(aaf_core::Error::Config(format!(
                    "roles.json: {name}: rank must be lead or worker"
                )));
            }
            roles.insert(
                name,
                RoleSpec {
                    driver: r.driver,
                    rank: r.rank,
                    workdir,
                    room_lanes: r.room_lanes,
                    room_overrides: overrides,
                },
            );
        }
        Ok(Self {
            roles,
            instance: home.to_path_buf(),
            agents: None,
            source: None,
        })
    }

    /// 讀 `<home>/deploy/roles.json`；檔案不存在時回空名單（仍記住路徑，之後建立也會被載入）。
    pub fn load(home: &std::path::Path) -> aaf_core::Result<Self> {
        let path = home.join("deploy").join("roles.json");
        let mut r = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| aaf_core::Error::Config(format!("{}: {e}", path.display())))?;
            Self::from_json(&text, home)?
        } else {
            Self {
                instance: home.to_path_buf(),
                ..Default::default()
            }
        };
        r.source = Some(path);
        Ok(r)
    }

    pub fn diff(&self, new: &Roster) -> RosterDiff {
        let mut d = RosterDiff::default();
        for (n, r) in &new.roles {
            match self.roles.get(n) {
                None => d.added.push(n.clone()),
                Some(old) if old == r => {}
                Some(old) => {
                    let same_base = old.driver == r.driver
                        && old.rank == r.rank
                        && old.workdir == r.workdir
                        && old.room_lanes == r.room_lanes;
                    if !same_base {
                        d.changed.push(n.clone());
                        continue;
                    }
                    // 只有覆寫不同：列出受影響的群
                    let rooms: std::collections::BTreeSet<RoomId> = old
                        .room_overrides
                        .keys()
                        .chain(r.room_overrides.keys())
                        .copied()
                        .collect();
                    for room in rooms {
                        if old.room_overrides.get(&room) != r.room_overrides.get(&room) {
                            d.lanes.push(Lane::new(n, Some(room)));
                        }
                    }
                }
            }
        }
        for n in self.roles.keys() {
            if !new.roles.contains_key(n) {
                d.removed.push(n.clone());
            }
        }
        d.added.sort();
        d.removed.sort();
        d.changed.sort();
        d
    }

    /// 這條 lane 實際用的 driver（全域＋本群覆寫）。
    pub fn spec_for(&self, lane: &Lane) -> Option<DriverSpec> {
        self.spec_for_on(lane, &today())
    }

    pub fn spec_for_on(&self, lane: &Lane, today: &str) -> Option<DriverSpec> {
        let r = self.roles.get(&lane.role)?;
        Some(
            match lane.room.and_then(|room| r.room_overrides.get(&room)) {
                Some(o) => o.apply(&r.driver, today),
                None => r.driver.clone(),
            },
        )
    }

    /// 本群 YOLO 是否生效（權限請求一律自動允許）。
    pub fn yolo(&self, lane: &Lane) -> bool {
        let Some(room) = lane.room else { return false };
        self.roles
            .get(&lane.role)
            .and_then(|r| r.room_overrides.get(&room))
            .is_some_and(|o| o.yolo_on(&today()))
    }

    /// 這封信實際歸屬的 lane（角色不開每群 lane 時一律回預設）。
    pub fn lane_for(&self, role: &str, room: Option<RoomId>) -> Lane {
        let per_room = self.roles.get(role).is_some_and(|r| r.room_lanes);
        Lane::new(role, if per_room { room } else { None })
    }
}

struct LaneState {
    driver: Arc<dyn Driver>,
    sanitizer: crate::sanitize::Sanitizer,
    recovery_pending: Arc<std::sync::atomic::AtomicBool>,
    context_fault: Arc<std::sync::Mutex<Option<String>>>,
    flush: crate::continuity::Flush,
    recording_closed: Arc<std::sync::atomic::AtomicBool>,
    last_active: Instant,
    /// 已送給此 lane 的長內容指紋
    context_fp: Option<u64>,
    /// 此 session 已開始的回合數（0＝新 session）
    turns: u64,
    forward: tokio::task::JoinHandle<()>,
    /// 最近一次收到 driver 事件的時間（卡死偵測用）
    last_event: Arc<std::sync::Mutex<Instant>>,
    /// 按了停止的時間；寬限期過後仍未停下就強制關閉 lane
    cancel_at: Option<Instant>,
}

type Factory = Arc<dyn Fn(&Lane) -> Option<Arc<dyn Driver>> + Send + Sync>;
/// 由上層（feature）提供的每 lane 額外環境變數（例如角色 API token）。
pub type EnvProvider = Arc<dyn Fn(&Lane) -> Vec<(String, String)> + Send + Sync>;
/// 由上層提供的提示前言（角色說明、可用指令）。
pub type Preamble = Arc<dyn Fn(&Lane) -> String + Send + Sync>;
/// 長內容（例如指派的 skill 全文）：回傳 (指紋, 內容)。同一 lane 只在第一回合或指紋改變時送出，
/// 不每回合重送（ACP 沒有獨立的系統提示）。
pub type ContextProvider = Arc<dyn Fn(&Lane) -> Option<(u64, String)> + Send + Sync>;
/// 權限自動核准：回傳 Some(理由) 就自動選第一個 allow_once（沒有則 allow_always）選項；None 交給人。
pub type AutoApprove = Arc<dyn Fn(&Lane, &str) -> Option<String> + Send + Sync>;

pub struct Dispatcher {
    pub inbox: Arc<dyn Inbox>,
    pub bus: EventBus,
    roster: SharedRoster,
    /// 設定已變更、正在收尾的角色：不派新回合，等進行中的回合結束後關閉其所有 lane。
    draining: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// 覆寫變了、正在收尾的單一 lane（只重開它，不動角色其他群）。
    draining_lanes: std::sync::Mutex<std::collections::BTreeSet<Lane>>,
    /// 每條 lane 建立時套用的 YOLO 期限狀態（跨日到期時重開 lane 換回一般模式）
    yolo_seen: std::sync::Mutex<HashMap<Lane, bool>>,
    pub idle_park: Duration,
    /// 按停止後等多久仍在忙就強制關閉 lane（停掉程序、保留 session）
    pub stop_grace: Duration,
    /// 忙碌中超過多久沒有任何事件就視為卡死並強制關閉；ZERO＝關閉
    pub stall_kill: Duration,
    lanes: Mutex<HashMap<Lane, LaneState>>,
    /// Serializes selection/start with lifecycle replacement; never held by a running turn.
    lifecycle: Mutex<()>,
    /// 已派出、尚未結束的回合（防止同一 lane 被重複派工）。
    in_flight: Arc<std::sync::Mutex<std::collections::HashSet<Lane>>>,
    /// 被 `reap_stuck` 強制關閉的 lane：進行中的回合因此失敗時視同取消（ack、不重跑）
    /// 值為原因（cancel_ignored／no_events）。
    force_closed: Arc<std::sync::Mutex<HashMap<Lane, &'static str>>>,
    /// 測試／特殊 driver 注入：回傳 Some 就用它，不經 DriverSpec 建立。
    factory: Option<Factory>,
    env_provider: Option<EnvProvider>,
    preamble: Option<Preamble>,
    context: Option<ContextProvider>,
    continuity: Option<Arc<dyn crate::continuity::Continuity>>,
    auto_approve: Option<AutoApprove>,
    hooks: Option<crate::hooks::Hooks>,
    /// 回合失敗後自動重派的次數（0＝不重試、直接公告）
    pub turn_retry: u32,
    /// 第 n 次重試前的等待（超出長度沿用最後一段）
    pub retry_backoff: Vec<Duration>,
    retries: Retries,
    /// driver 設定錯誤（建不起來）的 lane：派工時直接公告
    config_failed: std::sync::Mutex<std::collections::HashSet<Lane>>,
    /// 失敗公告要 @ 的對象（例如本群帶隊）；回 None 就不 @
    notice_mention: Option<NoticeMention>,
    /// 使用者按停止時在群裡公告沒處理完的訊息（AAF_STOP_NOTICE，預設開；false＝安靜 ack）
    pub stop_notice: bool,
    /// 回合進行中收到新信時用 Hermes `/steer` 即時插入（AAF_STEER，預設開）
    pub steer: bool,
    /// 收件 lane 忙超過多久、新信又插不進去時，在群裡公告「忙碌中、已排隊」；ZERO＝關閉
    pub busy_notice_after: Duration,
    /// 進行中回合的資訊（本輪批次、開始時間、已插入失敗／已公告的信）
    turn_meta: Arc<std::sync::Mutex<HashMap<Lane, TurnMeta>>>,
    /// 正在送 `/steer` 的 lane（期間不另派新回合）
    steering: Arc<std::sync::Mutex<std::collections::HashSet<Lane>>>,
    /// 已記錄過「被擋住、待 owner 解除」的 lane（每個 lane 只記一次 log，解除後清掉）
    blocked_noticed: std::sync::Mutex<std::collections::HashSet<Lane>>,
    /// 服務關機中（[`Dispatcher::shutdown`]）：不再派新回合
    shutting_down: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct TurnMeta {
    batch: Vec<i64>,
    started: Option<Instant>,
    steer_failed: std::collections::HashSet<i64>,
    noticed: std::collections::HashSet<i64>,
}

/// 回合結束（含提早 return）時清掉該 lane 的回合資訊。
struct TurnMetaGuard(Arc<std::sync::Mutex<HashMap<Lane, TurnMeta>>>, Lane);

impl Drop for TurnMetaGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.0.lock() {
            m.remove(&self.1);
        }
    }
}

/// 插入進行中回合時的提示（compose_prompt 加一行說明）。
pub fn compose_steer(lane: &Lane, msgs: &[InboxMessage]) -> String {
    format!(
        "[aaf] Delivered while your current turn is still running — read now, no need to wait for the turn to end. Reply to these in the room (`aaf chat post`) when appropriate.\n{}",
        compose_prompt(lane, msgs)
    )
}

/// 失敗公告要 @ 誰（上層依群的帶隊決定）。
pub type NoticeMention = Arc<dyn Fn(&Lane) -> Option<String> + Send + Sync>;

#[derive(Default)]
struct RetryState {
    attempts: u32,
    not_before: Option<Instant>,
}
type Retries = Arc<std::sync::Mutex<HashMap<Lane, RetryState>>>;

/// 回合失敗的處理：可重試且還有次數 → 退避後重派；否則貼系統公告，寫入成功才 ack。
#[derive(Clone)]
struct FailureCtx {
    inbox: Arc<dyn Inbox>,
    bus: EventBus,
    retries: Retries,
    turn_retry: u32,
    backoff: Vec<Duration>,
    mention: Option<NoticeMention>,
    stop_notice: bool,
}

impl FailureCtx {
    /// 使用者按停止（取消或停止後強制關閉）：不重跑，但在群裡留系統公告列出沒處理完的訊息；
    /// 批次中有其他角色寄來的信時 @ 那些作者請他們重送（被停止的角色本身不 @，避免又被叫醒）。
    /// 公告確實寫入才 ack；寫不進去就留在佇列並記 `runtime.lane.notice_failed`（不能無聲消失）。
    async fn stopped(
        &self,
        lane: &Lane,
        msgs: &[InboxMessage],
        roles: &std::collections::BTreeSet<String>,
    ) -> aaf_core::Result<()> {
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        if !self.stop_notice {
            return self.inbox.ack(&lane.role, &ids).await;
        }
        let list = ids
            .iter()
            .map(|i| format!("#{i}"))
            .collect::<Vec<_>>()
            .join("、");
        let mut authors: Vec<&str> = Vec::new();
        for m in msgs {
            if m.from != lane.role && roles.contains(&m.from) && !authors.contains(&m.from.as_str())
            {
                authors.push(&m.from);
            }
        }
        let mut text = format!(
            "⏹ 系統：{} 這一輪被手動停止；{list} 沒有處理完，需要的話請重新交代。",
            lane.role
        );
        if !authors.is_empty() {
            let at = authors
                .iter()
                .map(|a| format!("@{a}"))
                .collect::<Vec<_>>()
                .join(" ");
            text.push_str(&format!("{at} 你們寄的訊息對方可能沒讀到，請視需要重送。"));
        }
        match self.inbox.notify(lane.room, &text).await {
            Ok(Some(_)) => self.inbox.ack(&lane.role, &ids).await,
            other => {
                self.bus.publish(
                    "runtime.lane.notice_failed",
                    1,
                    json!({"lane": lane_json(lane), "messages": ids, "error": other.err().map(|e| e.to_string())}),
                )?;
                Ok(())
            }
        }
    }

    async fn fail(
        &self,
        lane: &Lane,
        ids: &[i64],
        reason: &str,
        retryable: bool,
    ) -> aaf_core::Result<()> {
        let reason: String = reason.replace('@', "＠").chars().take(200).collect();
        let attempt = {
            let mut r = self.retries.lock().unwrap();
            let st = r.entry(lane.clone()).or_default();
            if retryable && st.attempts < self.turn_retry {
                st.attempts += 1;
                let wait = self
                    .backoff
                    .get(st.attempts as usize - 1)
                    .or(self.backoff.last())
                    .copied()
                    .unwrap_or_default();
                st.not_before = Some(Instant::now() + wait);
                Some((st.attempts, wait))
            } else {
                r.remove(lane);
                None
            }
        };
        if let Some((n, wait)) = attempt {
            self.bus.publish(
                "runtime.turn.failed",
                1,
                json!({"lane": lane_json(lane), "messages": ids, "reason": reason, "attempt": n, "retry_in_ms": wait.as_millis() as u64, "final": false}),
            )?;
            return Ok(());
        }
        self.bus.publish(
            "runtime.turn.failed",
            1,
            json!({"lane": lane_json(lane), "messages": ids, "reason": reason, "final": true}),
        )?;
        let list = ids
            .iter()
            .map(|i| format!("#{i}"))
            .collect::<Vec<_>>()
            .join("、");
        let who = self
            .mention
            .as_ref()
            .and_then(|f| f(lane))
            .filter(|m| m != &lane.role)
            .map(|m| format!("@{m} "))
            .unwrap_or_default();
        let text = format!(
            "{who}⚠ 系統：{} 這一輪沒有完成（{reason}）；{list} 沒有做完，需要的話請重新交代。",
            lane.role
        );
        match self.inbox.notify(lane.room, &text).await {
            Ok(Some(_)) => self.inbox.ack(&lane.role, ids).await?,
            other => {
                self.bus.publish(
                    "runtime.lane.notice_failed",
                    1,
                    json!({"lane": lane_json(lane), "messages": ids, "error": other.err().map(|e| e.to_string())}),
                )?;
            }
        }
        Ok(())
    }
}

/// Hermes 用完重試後以「正常回覆」送回的錯誤說明（`agent/turn_failure_copy.py`）。
/// (片語, 可重試)。只比對短回覆的開頭，避免把討論錯誤的正常回覆誤判。
const HERMES_FAILURE_COPY: &[(&str, bool)] = &[
    ("rejected this request as malformed", true),
    ("didn't answer after", true),
    ("rate-limited every one of", true),
    ("is rate-limiting requests right now", true),
    ("reported it was overloaded on all", true),
    ("returned a server error on all", true),
    ("didn't respond in time on any of", true),
    ("sent back an empty or broken reply", true),
    ("Hermes hit repeated errors and stopped this turn", true),
    ("Hermes was shutting down and stopped this turn", true),
    ("Hermes hit an internal error", true),
    ("kept dropping while the model was writing", true),
    ("kept closing the stream before the model finished", true),
    (
        "requires user and assistant turns to strictly alternate",
        false,
    ),
    ("isn't available on", false),
    ("rejected your API key", false),
    ("couldn't verify", false),
    ("refused this request because of a policy", false),
    // 供應商內容／資安過濾器擋下（例如 Copilot HTTP 422 cybersecurity flag）：重送同內容仍會被擋
    ("safety filter refused this request", false),
    ("blocked the request before it reached the model", false),
];

/// 回覆是不是 Hermes 的錯誤說明：回傳 Some(可重試)。
pub fn hermes_failure_copy(text: &str) -> Option<bool> {
    let t = text.trim();
    if t.chars().count() > 800 {
        return None;
    }
    let head: String = t.chars().take(200).collect();
    HERMES_FAILURE_COPY
        .iter()
        .find(|(p, _)| head.contains(p))
        .map(|(_, r)| *r)
}

/// Codex 連線失敗時把警告當成「正常回覆」送回，例如
/// `Warning: Falling back from WebSockets to HTTPS transport. request timed out\n\n(empty)`。
/// 只有警告、後面沒有真正內容（空、`(empty)` 或重複同一個錯誤）才算失敗，一律可重試；
/// 警告後面接著真正回覆的不攔。
pub fn codex_failure_copy(text: &str) -> Option<bool> {
    const PREFIX: &str = "Warning: Falling back from WebSockets to HTTPS transport.";
    let t = text.trim();
    let after = t.strip_prefix(PREFIX)?;
    let (reason, rest) = after.split_once('\n').unwrap_or((after, ""));
    let (reason, rest) = (reason.trim(), rest.trim());
    (rest.is_empty() || rest == EMPTY_REPLY_MARKER || rest == reason).then_some(true)
}

/// 回覆是不是引擎的錯誤說明（Hermes 或 Codex）：回傳 Some(可重試)。
pub fn failure_copy(text: &str) -> Option<bool> {
    hermes_failure_copy(text).or_else(|| codex_failure_copy(text))
}

/// 錯誤說明的第一句（公告用；過長截斷並標 …）。
fn first_line(t: &str) -> String {
    let l = t.trim().lines().next().unwrap_or_default();
    let l = l.split_inclusive(". ").next().unwrap_or(l).trim();
    if l.chars().count() > 160 {
        format!("{}…", l.chars().take(160).collect::<String>())
    } else {
        l.to_string()
    }
}

/// 一輪提示的組成（之後可加 persona、team rules、skills）。
pub fn compose_prompt(lane: &Lane, msgs: &[InboxMessage]) -> String {
    let place = match lane.room {
        Some(r) => format!(" in room #{r}"),
        None => String::new(),
    };
    let mut s = format!(
        "You are role `{}`{place}. You have {} new message(s):\n",
        lane.role,
        msgs.len()
    );
    for m in msgs {
        s.push_str(&format!("\n[#{} from {}]\n{}\n", m.id, m.from, m.body));
    }
    s
}

fn event_kind(ev: &DriverEvent) -> &'static str {
    match ev {
        DriverEvent::Status { .. } => "runtime.agent.status",
        DriverEvent::MessageChunk { .. } | DriverEvent::ThoughtChunk { .. } => {
            "runtime.agent.stream"
        }
        DriverEvent::ToolCall { .. } => "runtime.agent.tool_call",
        DriverEvent::ToolEvidence { .. } => "runtime.agent.tool_evidence",
        DriverEvent::PermissionRequested { .. } => "runtime.permission.requested",
        DriverEvent::PermissionResolved { .. } => "runtime.permission.resolved",
        DriverEvent::TurnEnded { .. } => "runtime.turn.ended",
    }
}

/// 換 session 前請 agent 寫的交接（回覆整段存進 ledger，新 session 開頭注入）。
const HANDOFF_PROMPT: &str = "aaf: your context window is nearly full; this session will be replaced by a fresh one right after this reply. Do not call tools and do not continue the task. Write a handoff for your successor in under 600 words: the goal, what is done (with file paths, commit ids, numbers), what is in progress and the exact next step, open questions, and constraints or decisions that must not be lost. Write only the handoff.";
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(180);

fn lane_json(l: &Lane) -> serde_json::Value {
    json!({"role": l.role, "room": l.room})
}

impl Dispatcher {
    pub fn new(inbox: Arc<dyn Inbox>, bus: EventBus, roster: Roster) -> Self {
        Self::with_shared(inbox, bus, shared_roster(roster))
    }

    pub fn with_shared(inbox: Arc<dyn Inbox>, bus: EventBus, roster: SharedRoster) -> Self {
        Self {
            inbox,
            bus,
            roster,
            draining: Default::default(),
            draining_lanes: Default::default(),
            yolo_seen: Default::default(),
            idle_park: Duration::from_secs(24 * 3600),
            stop_grace: Duration::from_secs(10),
            stall_kill: Duration::from_secs(60 * 60),
            lanes: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
            in_flight: Default::default(),
            force_closed: Default::default(),
            factory: None,
            env_provider: None,
            preamble: None,
            context: None,
            continuity: None,
            auto_approve: None,
            hooks: None,
            turn_retry: 2,
            retry_backoff: vec![Duration::from_secs(30), Duration::from_secs(120)],
            retries: Default::default(),
            config_failed: Default::default(),
            notice_mention: None,
            stop_notice: true,
            steer: true,
            busy_notice_after: Duration::from_secs(10 * 60),
            turn_meta: Default::default(),
            steering: Default::default(),
            blocked_noticed: Default::default(),
            shutting_down: Default::default(),
        }
    }

    /// 目前生效的角色設定（快照）。
    pub fn roster(&self) -> Arc<Roster> {
        self.roster.read().unwrap().clone()
    }

    pub fn shared_roster(&self) -> SharedRoster {
        self.roster.clone()
    }

    /// 正在收尾、尚未套用新設定的角色。
    pub fn draining(&self) -> Vec<String> {
        self.draining.lock().unwrap().iter().cloned().collect()
    }

    /// 熱重載：新設定立即生效於新 lane；變更或移除的角色先停止派工，
    /// 進行中的回合自然結束後關閉並遺忘其 session（不同 agent／設定不續接舊 session），之後才以新設定啟動。
    pub async fn apply_roster(&self, mut new: Roster) -> aaf_core::Result<RosterDiff> {
        let _lifecycle = self.lifecycle.lock().await;
        let old = self.roster();
        if new.agents.is_none() {
            new.agents = old.agents.clone();
        }
        if new.source.is_none() {
            new.source = old.source.clone();
        }
        let diff = old.diff(&new);
        if diff.is_empty() {
            return Ok(diff);
        }
        {
            let mut dr = self.draining.lock().unwrap();
            for r in diff.changed.iter().chain(&diff.removed) {
                dr.insert(r.clone());
            }
            let mut dl = self.draining_lanes.lock().unwrap();
            for l in &diff.lanes {
                dl.insert(l.clone());
            }
        }
        *self.roster.write().unwrap() = Arc::new(new);
        for r in diff.changed.iter().chain(&diff.removed) {
            self.inbox.forget_sessions(r).await?;
        }
        for l in &diff.lanes {
            // 換了 agent／模型／模式：不續接舊 session
            self.inbox.save_session(l, "").await?;
        }
        self.bus
            .publish("runtime.roster.reloaded", 1, json!(diff))?;
        self.reconcile_locked().await?;
        Ok(diff)
    }

    /// agent 程式換了版本（套件升級）：所有實際用這個 agent 的開著 lane 排入收尾重開，
    /// 進行中的回合先跑完，之後遺忘舊 session、下回合用新程式。回傳受影響的 lane。
    pub async fn restart_agent(&self, agent: &str) -> aaf_core::Result<Vec<Lane>> {
        let _lifecycle = self.lifecycle.lock().await;
        let roster = self.roster();
        let open: Vec<Lane> = self.lanes.lock().await.keys().cloned().collect();
        let hit: Vec<Lane> = open
            .into_iter()
            .filter(|l| {
                matches!(roster.spec_for(l), Some(DriverSpec::Acp { acp_agent: Some(a), .. }) if a == agent)
            })
            .collect();
        for l in &hit {
            self.inbox.save_session(l, "").await?;
            self.draining_lanes.lock().unwrap().insert(l.clone());
        }
        if !hit.is_empty() {
            self.bus.publish(
                "runtime.agent.restarting",
                1,
                json!({"agent": agent, "lanes": hit.iter().map(lane_json).collect::<Vec<_>>()}),
            )?;
            self.reconcile_locked().await?;
        }
        Ok(hit)
    }

    /// 收尾：關閉 draining 角色中已不忙的 lane；角色的 lane 全關後解除 draining。
    pub async fn reconcile(&self) -> aaf_core::Result<Vec<String>> {
        let _lifecycle = self.lifecycle.lock().await;
        self.reconcile_locked().await
    }

    async fn reconcile_locked(&self) -> aaf_core::Result<Vec<String>> {
        // YOLO 跨日到期：開著的 lane 狀態與現在不同就排入重開
        {
            let roster = self.roster();
            let open: Vec<Lane> = self.lanes.lock().await.keys().cloned().collect();
            let seen = self.yolo_seen.lock().unwrap().clone();
            let mut dl = self.draining_lanes.lock().unwrap();
            for l in open {
                if seen.get(&l).copied().unwrap_or(false) != roster.yolo(&l) {
                    dl.insert(l);
                }
            }
        }
        let lanes: Vec<Lane> = self
            .draining_lanes
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        for l in lanes {
            if self.busy(&l) {
                continue;
            }
            let open = self.lanes.lock().await.contains_key(&l);
            if open {
                self.close_locked(&l, true).await?;
            }
            self.draining_lanes.lock().unwrap().remove(&l);
            self.bus.publish(
                "runtime.roster.applied",
                1,
                json!({"role": l.role, "room": l.room}),
            )?;
        }
        let roles = self.draining();
        let mut done = Vec::new();
        for role in roles {
            let open: Vec<Lane> = self
                .lanes
                .lock()
                .await
                .keys()
                .filter(|l| l.role == role)
                .cloned()
                .collect();
            let mut remaining = 0;
            for l in open {
                if self.busy(&l) {
                    remaining += 1;
                } else {
                    self.close_locked(&l, true).await?;
                }
            }
            if remaining == 0 {
                self.draining.lock().unwrap().remove(&role);
                self.bus
                    .publish("runtime.roster.applied", 1, json!({"role": role}))?;
                done.push(role);
            }
        }
        Ok(done)
    }

    pub fn with_env_provider(
        mut self,
        f: impl Fn(&Lane) -> Vec<(String, String)> + Send + Sync + 'static,
    ) -> Self {
        self.env_provider = Some(Arc::new(f));
        self
    }

    pub fn with_continuity(
        mut self,
        continuity: Option<Arc<dyn crate::continuity::Continuity>>,
    ) -> Self {
        self.continuity = continuity.filter(|c| c.enabled());
        self
    }

    pub fn with_stall_kill(mut self, d: Duration) -> Self {
        self.stall_kill = d;
        self
    }

    pub fn with_steer(mut self, on: bool) -> Self {
        self.steer = on;
        self
    }

    pub fn with_busy_notice(mut self, after: Duration) -> Self {
        self.busy_notice_after = after;
        self
    }

    fn is_hermes_lane(&self, lane: &Lane) -> bool {
        match self.roster().spec_for(lane) {
            Some(DriverSpec::Acp {
                acp_agent, command, ..
            }) => {
                acp_agent.as_deref() == Some("hermes")
                    || command.first().is_some_and(|c| {
                        // 只看執行檔名（路徑可能含 ~/.hermes 之類目錄）
                        std::path::Path::new(c)
                            .file_name()
                            .and_then(|f| f.to_str())
                            .is_some_and(|f| f.starts_with("hermes"))
                    })
            }
            _ => false,
        }
    }

    /// 收件 lane 正在跑回合時的新信：Hermes lane 用 `/steer` 即時插入（成功即 ack）；
    /// 插不進去（非 Hermes、等權限、插入失敗）且這輪已忙超過 `busy_notice_after`，
    /// 在群裡公告一次「忙碌中、已排隊」，讓寄件人知道不是已讀不回。
    async fn on_busy(
        &self,
        lane: &Lane,
    ) -> aaf_core::Result<Option<tokio::task::JoinHandle<aaf_core::Result<()>>>> {
        if self.steering.lock().unwrap().contains(lane) {
            return Ok(None);
        }
        let (batch, started, failed, noticed) = {
            let m = self.turn_meta.lock().unwrap();
            let Some(t) = m.get(lane) else {
                return Ok(None);
            };
            (
                t.batch.clone(),
                t.started,
                t.steer_failed.clone(),
                t.noticed.clone(),
            )
        };
        let fresh: Vec<InboxMessage> = self
            .inbox
            .unread(lane)
            .await?
            .into_iter()
            .filter(|m| !batch.contains(&m.id))
            .collect();
        if fresh.is_empty() {
            return Ok(None);
        }
        let stopping = self
            .lanes
            .lock()
            .await
            .get(lane)
            .is_some_and(|s| s.cancel_at.is_some());
        let driver = self.lane_driver(lane).await;
        let steerable: Vec<InboxMessage> = fresh
            .iter()
            .filter(|m| !failed.contains(&m.id))
            .cloned()
            .collect();
        if self.steer
            && !stopping
            && !steerable.is_empty()
            && self.is_hermes_lane(lane)
            && driver
                .as_ref()
                .is_some_and(|d| d.status() == AgentStatus::Working)
        {
            let d = driver.expect("checked");
            self.steering.lock().unwrap().insert(lane.clone());
            let (inbox, bus, steering, meta) = (
                self.inbox.clone(),
                self.bus.clone(),
                self.steering.clone(),
                self.turn_meta.clone(),
            );
            let lane = lane.clone();
            let continuity = self.continuity.clone();
            let context_fault = self.lanes.lock().await[&lane].context_fault.clone();
            let sanitizer = self.lanes.lock().await[&lane].sanitizer.clone();
            let flush = self.lanes.lock().await[&lane].flush.clone();
            let recording_closed = self.lanes.lock().await[&lane].recording_closed.clone();
            return Ok(Some(tokio::spawn(async move {
                let _g = InFlight(steering, lane.clone());
                if continuity.is_some() {
                    crate::continuity::flush_events(&flush, &recording_closed).await?;
                    if let Some(error) = context_fault.lock().unwrap().clone() {
                        return Err(aaf_core::Error::Other(error));
                    }
                }
                let ids: Vec<i64> = steerable.iter().map(|m| m.id).collect();
                let mut prepared = match &continuity {
                    Some(c) => c.prepare(&lane, false, &sanitizer.messages(&steerable))?,
                    None => Default::default(),
                };
                // Steering records the new input, but never re-injects a recovery package.
                prepared.text.clear();
                prepared.restoring = false;
                let r = d.steer(&compose_steer(&lane, &steerable)).await;
                if let Some(c) = &continuity {
                    let observation = match &r {
                        Ok(crate::driver::SteerOutcome::Turn(t)) => Ok(t.clone()),
                        Ok(crate::driver::SteerOutcome::Steered) => Ok(crate::driver::TurnResult {
                            text: String::new(),
                            stop_reason: "steering_accepted".into(),
                        }),
                        other => Err(crate::driver::DriverError::Protocol(format!(
                            "steering not confirmed: {other:?}"
                        ))),
                    };
                    let committed: aaf_core::Result<()> = async {
                        crate::continuity::flush_events(&flush, &recording_closed).await?;
                        c.complete(&lane, &prepared, &sanitizer.result(&observation))
                    }
                    .await;
                    if let Err(e) = committed {
                        *context_fault.lock().unwrap() = Some(e.to_string());
                        let _ = c.failed(&lane, &e.to_string());
                        return Err(e);
                    }
                }
                match r {
                    Ok(crate::driver::SteerOutcome::Steered) => {
                        inbox.ack(&lane.role, &ids).await?;
                        bus.publish(
                            "runtime.turn.steered",
                            1,
                            json!({"lane": lane_json(&lane), "messages": ids}),
                        )?;
                    }
                    Ok(crate::driver::SteerOutcome::Turn(t)) => {
                        // 送到時回合剛結束，agent 把它當一般提示跑完：照一般回覆處理
                        let body = t.text.trim();
                        if t.stop_reason == "end_turn"
                            && !body.is_empty()
                            && body != EMPTY_REPLY_MARKER
                        {
                            let anchor = steerable.last().expect("non-empty");
                            inbox.reply(&lane.role, anchor, body).await?;
                        }
                        inbox.ack(&lane.role, &ids).await?;
                        bus.publish(
                            "runtime.turn.steered",
                            1,
                            json!({"lane": lane_json(&lane), "messages": ids, "ran_as_turn": true, "stop_reason": t.stop_reason}),
                        )?;
                    }
                    Ok(crate::driver::SteerOutcome::NotRunning)
                    | Ok(crate::driver::SteerOutcome::Unsupported) => {}
                    Ok(crate::driver::SteerOutcome::Failed(e)) => {
                        if let Some(t) = meta.lock().unwrap().get_mut(&lane) {
                            t.steer_failed.extend(ids.iter().copied());
                        }
                        bus.publish(
                            "runtime.turn.steer_failed",
                            1,
                            json!({"lane": lane_json(&lane), "messages": ids, "error": e}),
                        )?;
                    }
                    Err(e) => {
                        if let Some(t) = meta.lock().unwrap().get_mut(&lane) {
                            t.steer_failed.extend(ids.iter().copied());
                        }
                        bus.publish(
                            "runtime.turn.steer_failed",
                            1,
                            json!({"lane": lane_json(&lane), "messages": ids, "error": e.to_string()}),
                        )?;
                    }
                }
                Ok(())
            })));
        }
        // 插不進去：忙得夠久才公告，同一封信只公告一次
        let Some(started) = started else {
            return Ok(None);
        };
        if self.busy_notice_after.is_zero() || started.elapsed() < self.busy_notice_after {
            return Ok(None);
        }
        let new: Vec<i64> = fresh
            .iter()
            .map(|m| m.id)
            .filter(|i| !noticed.contains(i))
            .collect();
        if new.is_empty() {
            return Ok(None);
        }
        let list = new
            .iter()
            .map(|i| format!("#{i}"))
            .collect::<Vec<_>>()
            .join("、");
        let mins = started.elapsed().as_secs() / 60;
        let text = format!(
            "⏳ 系統：{} 正在忙（這一輪已 {mins} 分鐘）；{list} 已排隊，這輪結束後才會讀到。急事可按停止。",
            lane.role
        );
        if let Ok(Some(_)) = self.inbox.notify(lane.room, &text).await {
            if let Some(t) = self.turn_meta.lock().unwrap().get_mut(lane) {
                t.noticed.extend(new.iter().copied());
            }
            self.bus.publish(
                "runtime.lane.busy_notice",
                1,
                json!({"lane": lane_json(lane), "messages": new}),
            )?;
        }
        Ok(None)
    }

    pub fn with_stop_notice(mut self, on: bool) -> Self {
        self.stop_notice = on;
        self
    }

    pub fn with_turn_retry(mut self, n: u32) -> Self {
        self.turn_retry = n;
        self
    }

    pub fn with_notice_mention(mut self, f: NoticeMention) -> Self {
        self.notice_mention = Some(f);
        self
    }

    fn failure_ctx(&self) -> FailureCtx {
        FailureCtx {
            inbox: self.inbox.clone(),
            bus: self.bus.clone(),
            retries: self.retries.clone(),
            turn_retry: self.turn_retry,
            backoff: self.retry_backoff.clone(),
            mention: self.notice_mention.clone(),
            stop_notice: self.stop_notice,
        }
    }

    pub fn with_hooks(mut self, h: crate::hooks::Hooks) -> Self {
        self.hooks = Some(h);
        self
    }

    pub fn with_hooks_opt(mut self, h: Option<crate::hooks::Hooks>) -> Self {
        self.hooks = h;
        self
    }

    pub fn with_auto_approve(mut self, f: AutoApprove) -> Self {
        self.auto_approve = Some(f);
        self
    }

    pub fn with_context(mut self, f: ContextProvider) -> Self {
        self.context = Some(f);
        self
    }

    pub fn with_preamble(mut self, f: impl Fn(&Lane) -> String + Send + Sync + 'static) -> Self {
        self.preamble = Some(Arc::new(f));
        self
    }

    pub fn with_factory(
        mut self,
        f: impl Fn(&Lane) -> Option<Arc<dyn Driver>> + Send + Sync + 'static,
    ) -> Self {
        self.factory = Some(Arc::new(f));
        self
    }

    /// 取得（必要時建立）lane 的 driver；manual 或未知角色回 None。
    async fn driver_for(&self, lane: &Lane) -> aaf_core::Result<Option<Arc<dyn Driver>>> {
        let mut lanes = self.lanes.lock().await;
        if let Some(s) = lanes.get(lane) {
            return Ok(Some(s.driver.clone()));
        }
        let roster = self.roster();
        let Some(spec) = roster.spec_for(lane) else {
            return Ok(None);
        };
        let yolo = roster.yolo(lane);
        self.yolo_seen.lock().unwrap().insert(lane.clone(), yolo);
        if spec.is_manual() {
            return Ok(None);
        }
        // Resolve once: issuing a role credential again can revoke the first value.
        let extra_env = self
            .env_provider
            .as_ref()
            .map(|f| f(lane))
            .unwrap_or_default();
        let driver: Arc<dyn Driver> = match self.factory.as_ref().and_then(|f| f(lane)) {
            Some(d) => d,
            None => {
                let resume = self
                    .inbox
                    .load_session(lane)
                    .await?
                    .filter(|s| !s.is_empty());
                let workdir = &roster.roles[&lane.role].workdir;
                let ctx = crate::driver::LaunchContext {
                    role: lane.role.clone(),
                    instance: roster.instance.clone(),
                    agents: roster.agents.clone(),
                    extra_env: extra_env.clone(),
                };
                match spec.build_resuming(workdir, resume, &ctx) {
                    Ok(d) => Arc::from(d),
                    Err(e) => {
                        tracing::warn!(%lane, error = %e, "invalid driver config");
                        self.config_failed.lock().unwrap().insert(lane.clone());
                        self.bus.publish(
                            "runtime.lane.config_error",
                            1,
                            json!({"lane": lane_json(lane), "error": e}),
                        )?;
                        return Ok(None);
                    }
                }
            }
        };
        let mut secret_env = extra_env;
        if let DriverSpec::Acp { env, .. } = &spec {
            secret_env.extend(env.clone());
        }
        let sanitizer = crate::sanitize::Sanitizer::from_env(&secret_env);
        let last_event = Arc::new(std::sync::Mutex::new(Instant::now()));
        let context_fault = Arc::new(std::sync::Mutex::new(None::<String>));
        let (flush, mut flush_rx) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<aaf_core::Result<()>>>(1);
        let forward = {
            let (lane, bus, mut rx) = (lane.clone(), self.bus.clone(), driver.events());
            let seen = last_event.clone();
            let (auto, drv) = (self.auto_approve.clone(), Arc::downgrade(&driver));
            let shared = self.roster.clone();
            let continuity = self.continuity.clone();
            let sanitizer = sanitizer.clone();
            let fault = context_fault.clone();
            tokio::spawn(async move {
                let mut failure: Option<String> = None;
                // Memory is the admission latch even when durable storage is unavailable.
                let failed = |message: String| {
                    *fault.lock().unwrap() = Some(message.clone());
                    if let Some(c) = &continuity {
                        if let Err(e) = c.failed(&lane, &message) {
                            tracing::warn!(%lane, error = %e, "context fault persistence failed; lane blocked in memory");
                        }
                    }
                };
                loop {
                    let next = tokio::select! {
                        biased;
                        ev = rx.recv() => ev,
                        Some(reply) = flush_rx.recv() => {
                            let _ = reply.send(match &failure { Some(e) => Err(aaf_core::Error::Other(e.clone())), None => Ok(()) });
                            continue;
                        }
                    };
                    match next {
                        Ok(ev) => {
                            *seen.lock().unwrap() = Instant::now();
                            if continuity.is_none()
                                && matches!(ev, DriverEvent::ToolEvidence { .. })
                            {
                                continue;
                            }
                            let mut safe =
                                serde_json::to_value(&ev).expect("driver event serializes");
                            // Keep legacy mode unchanged; status has no credential-bearing text.
                            if continuity.is_some() && !matches!(ev, DriverEvent::Status { .. }) {
                                sanitizer.value(&mut safe);
                            }
                            let ev: DriverEvent = serde_json::from_value(safe)
                                .expect("sanitizer retains event shape");
                            let published = bus.publish(
                                event_kind(&ev),
                                1,
                                json!({"lane": lane_json(&lane), "event": ev}),
                            );
                            if let Some(c) = &continuity {
                                let recorded =
                                    published.and_then(|e| c.observed(&lane, e.seq, &ev));
                                if let Err(e) = recorded {
                                    failure = Some(e.to_string());
                                    failed(e.to_string());
                                }
                            }
                            // 自動核准：只選 allow 類選項；沒有就交給人
                            let none: AutoApprove = Arc::new(|_: &Lane, _: &str| None);
                            if let (
                                Some(f),
                                DriverEvent::PermissionRequested {
                                    request_id,
                                    title,
                                    options,
                                },
                            ) = (auto.as_ref().or(Some(&none)), &ev)
                            {
                                let yolo = shared.read().unwrap().yolo(&lane);
                                let pick = if yolo {
                                    options
                                        .iter()
                                        .find(|o| o.kind == "allow_always")
                                        .or_else(|| options.iter().find(|o| o.kind == "allow_once"))
                                } else {
                                    options.iter().find(|o| o.kind == "allow_once").or_else(|| {
                                        options.iter().find(|o| o.kind == "allow_always")
                                    })
                                };
                                let reason = if yolo {
                                    Some(format!("room {} YOLO", lane.room.unwrap_or_default()))
                                } else {
                                    f(&lane, title)
                                };
                                if let (Some(reason), Some(opt), Some(d)) =
                                    (reason, pick, drv.upgrade())
                                {
                                    let ok = d
                                        .respond_permission(
                                            *request_id,
                                            crate::driver::PermissionDecision::Selected {
                                                option_id: opt.id.clone(),
                                            },
                                        )
                                        .await
                                        .is_ok();
                                    let _ = bus.publish(
                                        "runtime.permission.auto_approved",
                                        1,
                                        json!({"lane": lane_json(&lane), "request_id": request_id, "title": title, "option": opt.id, "reason": reason, "ok": ok}),
                                    );
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            if continuity.is_some() {
                                let message =
                                    format!("context event coverage gap: lost {n} driver events");
                                failure = Some(message.clone());
                                failed(message);
                            }
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        lanes.insert(
            lane.clone(),
            LaneState {
                driver: driver.clone(),
                sanitizer,
                recovery_pending: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                context_fault,
                recording_closed: Default::default(),
                flush,
                last_active: Instant::now(),
                context_fp: None,
                turns: 0,
                forward,
                last_event,
                cancel_at: None,
            },
        );
        Ok(Some(driver))
    }

    /// 目前開著的 lane 與狀態（狀態側欄、session 列表用）。
    pub async fn lane_states(&self) -> Vec<(Lane, AgentStatus)> {
        let mut v: Vec<_> = self
            .lanes
            .lock()
            .await
            .iter()
            .map(|(l, s)| (l.clone(), s.driver.status()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// 某 lane 的 driver（權限卡片按鈕要回應權限時用）。
    pub async fn lane_driver(&self, lane: &Lane) -> Option<Arc<dyn Driver>> {
        self.lanes.lock().await.get(lane).map(|s| s.driver.clone())
    }

    /// 取消某 lane 的當前回合（停止按鈕）。回傳該 lane 是否存在。
    /// agent 不理會取消時（例如卡在已斷線的模型串流），寬限期過後由 `reap_stuck` 強制關閉。
    pub async fn cancel(&self, lane: &Lane) -> aaf_core::Result<bool> {
        let Some(d) = self.lane_driver(lane).await else {
            return Ok(false);
        };
        if let Some(s) = self.lanes.lock().await.get_mut(lane) {
            s.cancel_at.get_or_insert_with(Instant::now);
        }
        d.cancel()
            .await
            .map_err(|e| aaf_core::Error::Other(e.to_string()))?;
        Ok(true)
    }

    /// 強制關閉卡住的 lane（停掉程序、保留 session id，下次有信時續接）：
    /// - 按了停止、過了 `stop_grace` 仍在 Working；
    /// - Working 中超過 `stall_kill` 沒有任何事件。
    ///
    /// 等人回應權限（Blocked）不算卡住。回傳被關閉的 lane。
    pub async fn reap_stuck(&self) -> aaf_core::Result<Vec<Lane>> {
        let _lifecycle = self.lifecycle.lock().await;
        let stuck: Vec<(Lane, &'static str)> = {
            let mut lanes = self.lanes.lock().await;
            let mut v = Vec::new();
            for (l, s) in lanes.iter_mut() {
                if s.driver.status() != AgentStatus::Working {
                    s.cancel_at = None;
                    continue;
                }
                let quiet = s.last_event.lock().unwrap().elapsed();
                if s.cancel_at.is_some_and(|t| t.elapsed() >= self.stop_grace) {
                    v.push((l.clone(), "cancel_ignored"));
                } else if !self.stall_kill.is_zero() && quiet >= self.stall_kill {
                    v.push((l.clone(), "no_events"));
                }
            }
            v
        };
        for (l, reason) in &stuck {
            tracing::warn!(lane = %l, reason, "force-closing stuck lane");
            if self.busy(l) {
                self.force_closed.lock().unwrap().insert(l.clone(), reason);
            }
            self.close_locked(l, false).await?;
            self.bus.publish(
                "runtime.lane.force_closed",
                1,
                json!({"lane": lane_json(l), "reason": reason}),
            )?;
        }
        Ok(stuck.into_iter().map(|(l, _)| l).collect())
    }

    /// 關閉某 lane（session 列表「關閉」、刪除群時）；forget=true 連 session id 都清掉，不再續接。
    pub async fn close(&self, lane: &Lane, forget: bool) -> aaf_core::Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.continuity.is_some() && self.busy(lane) {
            return Err(aaf_core::Error::Other(
                "turn in progress; stop it first".into(),
            ));
        }
        self.close_locked(lane, forget).await?;
        if let Some(c) = &self.continuity {
            c.recover(lane)?;
        }
        self.blocked_noticed.lock().unwrap().remove(lane);
        Ok(())
    }

    /// lane 被擋住、要 owner 明確解除時回傳原因（重啟後殘留未提交的嘗試、context 錯誤）；
    /// 正常或無法判斷時回 None。成員面板用來顯示「卡住」與解除按鈕。
    pub async fn blocked(&self, lane: &Lane) -> Option<String> {
        if self.busy(lane) {
            return None;
        }
        if let Some(e) = self
            .lanes
            .lock()
            .await
            .get(lane)
            .and_then(|s| s.context_fault.lock().unwrap().clone())
        {
            return Some(e);
        }
        let c = self.continuity.as_ref()?;
        match c.fault(lane) {
            Ok(e) => e,
            Err(e) => Some(e.to_string()),
        }
    }

    /// 派工時發現 lane 被擋住：每個 lane 記一次 log 並發事件，否則訊息只會靜靜排在佇列裡。
    fn notice_blocked(&self, lane: &Lane, error: &str) {
        if !self.blocked_noticed.lock().unwrap().insert(lane.clone()) {
            return;
        }
        tracing::warn!(%lane, %error, "lane blocked; owner must recover it (POST .../lanes/{{role}}/close?forget=false)");
        let _ = self.bus.publish(
            "runtime.lane.blocked",
            1,
            json!({"lane": lane_json(lane), "error": error}),
        );
    }

    /// 上下文快滿、要換 session 前：還叫得動就請 agent 自己寫交接（花一輪 token），
    /// 叫不動（額度用完、供應商錯誤、逾時）就跳過，新 session 只拿 ledger 的逐字摘錄。
    async fn write_handoff(&self, lane: &Lane, c: &dyn crate::continuity::Continuity) {
        let driver = self.lanes.lock().await.get(lane).map(|s| s.driver.clone());
        let Some(d) = driver else { return };
        if d.status() != AgentStatus::Idle {
            return;
        }
        let usage = d.context_usage();
        let r = tokio::time::timeout(HANDOFF_TIMEOUT, d.prompt(HANDOFF_PROMPT)).await;
        let (written, reason) = match r {
            Ok(Ok(t)) if t.stop_reason != "end_turn" => {
                (false, format!("stopReason={}", t.stop_reason))
            }
            Ok(Ok(t)) if t.text.trim().is_empty() => (false, "empty reply".into()),
            Ok(Ok(t)) => match failure_copy(&t.text)
                .map(|_| first_line(&t.text))
                .or_else(|| crate::catalog::provider_error(&t.text))
            {
                Some(e) => (false, e),
                None => match c.handoff(lane, t.text.trim()) {
                    Ok(()) => (true, String::new()),
                    Err(e) => (false, e.to_string()),
                },
            },
            Ok(Err(e)) => (false, e.to_string()),
            Err(_) => (
                false,
                format!("timed out after {}s", HANDOFF_TIMEOUT.as_secs()),
            ),
        };
        let _ = self.bus.publish(
            "runtime.context.handoff",
            1,
            json!({"lane": lane_json(lane), "written": written, "reason": reason,
                   "used": usage.as_ref().map(|u| u.used), "size": usage.as_ref().map(|u| u.size)}),
        );
    }

    async fn close_locked(&self, lane: &Lane, forget: bool) -> aaf_core::Result<()> {
        let owned = self.lanes.lock().await.get(lane).map(|s| {
            (
                s.driver.clone(),
                s.flush.clone(),
                s.recording_closed.clone(),
                s.context_fault.clone(),
            )
        });
        if let Some((driver, flush, recording_closed, fault)) = owned {
            let stopped = driver.stop().await;
            // Even a refused stop can emit final evidence. Keep its subscription and ownership.
            let drained = if self.continuity.is_some() {
                crate::continuity::flush_events(&flush, &recording_closed).await
            } else {
                Ok(())
            };
            if let Err(e) = stopped {
                *fault.lock().unwrap() = Some(e.to_string());
                if let Some(c) = &self.continuity {
                    let _ = c.failed(lane, &e.to_string());
                }
                return Err(aaf_core::Error::Other(format!(
                    "executor stop unverified; ownership retained: {e}"
                )));
            }
            if let Err(e) = &drained {
                *fault.lock().unwrap() = Some(e.to_string());
                // Persist before removing the only in-memory admission block.
                if let Some(c) = &self.continuity {
                    c.failed(lane, &e.to_string())?;
                }
            }
            let s = self
                .lanes
                .lock()
                .await
                .remove(lane)
                .expect("lifecycle lock owns executor");
            self.yolo_seen.lock().unwrap().remove(lane);
            if drained.is_ok() {
                s.recording_closed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            s.forward.abort();
            drained?;
            if !forget {
                if let Some(sid) = s.driver.session_id() {
                    self.inbox.save_session(lane, &sid).await?;
                }
            }
            self.bus.publish(
                "runtime.lane.closed",
                1,
                json!({"lane": lane_json(lane), "forget": forget}),
            )?;
        }
        if forget {
            self.inbox.save_session(lane, "").await?;
        }
        Ok(())
    }

    /// 服務關機：不再派工，停掉所有 lane 的 agent（整個程序群組），保留 session id 供重啟後續接。
    /// 進行中的回合視為被關機中斷：訊息不 ack、不公告、不重試，留在佇列等重啟後照常處理
    /// （有未提交的 attempt 時仍由 owner 解除 lane，見 docs/context.md）。回傳停不掉的 lane 與原因。
    pub async fn shutdown(&self) -> Vec<(Lane, String)> {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _lifecycle = self.lifecycle.lock().await;
        let ls: Vec<Lane> = self.lanes.lock().await.keys().cloned().collect();
        let mut failed = Vec::new();
        for l in &ls {
            if self.busy(l) {
                self.force_closed
                    .lock()
                    .unwrap()
                    .insert(l.clone(), "shutdown");
            }
            if let Err(e) = self.close_locked(l, false).await {
                failed.push((l.clone(), e.to_string()));
            }
        }
        // agent 已停：等回合 task 寫完收尾（attempt 結果、事件），程序結束前不要把它們砍在半路
        let drained = tokio::time::timeout(SHUTDOWN_TURN_DRAIN, async {
            while !self.in_flight.lock().unwrap().is_empty()
                || !self.steering.lock().unwrap().is_empty()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                "turns still finishing after {}s; exiting anyway",
                SHUTDOWN_TURN_DRAIN.as_secs()
            );
        }
        failed
    }

    /// 關閉某群的所有 lane（刪除群時用）。
    pub async fn close_room(&self, room: RoomId, forget: bool) -> aaf_core::Result<Vec<Lane>> {
        let _lifecycle = self.lifecycle.lock().await;
        let ls: Vec<Lane> = self
            .lanes
            .lock()
            .await
            .keys()
            .filter(|l| l.room == Some(room))
            .cloned()
            .collect();
        for l in &ls {
            self.close_locked(l, forget).await?;
        }
        Ok(ls)
    }

    /// 收起閒置過久的 lane（session id 已存，之後續接）。
    pub async fn park_idle(&self) -> aaf_core::Result<Vec<Lane>> {
        let _lifecycle = self.lifecycle.lock().await;
        let idle: Vec<Lane> = self
            .lanes
            .lock()
            .await
            .iter()
            .filter(|(l, s)| {
                !self.busy(l)
                    && s.context_fault.lock().unwrap().is_none()
                    && self
                        .continuity
                        .as_ref()
                        .is_none_or(|c| matches!(c.fault(l), Ok(None)))
                    && s.last_active.elapsed() >= self.idle_park
                    && matches!(s.driver.status(), AgentStatus::Idle | AgentStatus::Offline)
            })
            .map(|(l, _)| l.clone())
            .collect();
        for l in &idle {
            self.close_locked(l, false).await?;
        }
        Ok(idle)
    }

    /// 跑一輪並等所有回合結束（測試、CLI 用）。回傳本輪喚醒的 lane。
    pub async fn once(&self) -> aaf_core::Result<Vec<Lane>> {
        let (woke, tasks, mut error) = self.launch().await?;
        for t in tasks {
            if let Err(e) = t
                .await
                .map_err(|e| aaf_core::Error::Other(e.to_string()))
                .and_then(|r| r)
            {
                error.get_or_insert(e);
            }
        }
        match error {
            Some(e) => Err(e),
            None => Ok(woke),
        }
    }

    /// 派工但不等待（伺服器迴圈用）：慢的 lane 不會擋住其他 lane 的新信。
    pub async fn tick(&self) -> aaf_core::Result<Vec<Lane>> {
        let (woke, _tasks, error) = self.launch().await?;
        match error {
            Some(e) => Err(e),
            None => Ok(woke),
        }
    }

    /// 是否有進行中的回合（刪除群前檢查）。
    pub fn busy(&self, lane: &Lane) -> bool {
        self.in_flight.lock().unwrap().contains(lane)
            || self.steering.lock().unwrap().contains(lane)
    }

    pub fn busy_in_room(&self, room: RoomId) -> bool {
        self.in_flight
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.room == Some(room))
    }

    #[allow(clippy::type_complexity)]
    async fn launch(
        &self,
    ) -> aaf_core::Result<(
        Vec<Lane>,
        Vec<tokio::task::JoinHandle<aaf_core::Result<()>>>,
        Option<aaf_core::Error>,
    )> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok((Vec::new(), Vec::new(), None));
        }
        let mut error = self.reconcile_locked().await.err();
        let draining = self.draining();
        let mut lanes = self.inbox.lanes_with_unread().await?;
        lanes.sort();
        lanes.dedup();
        let dl = self.draining_lanes.lock().unwrap().clone();
        lanes.retain(|l| !draining.contains(&l.role) && !dl.contains(l));
        let mut woke = Vec::new();
        let mut tasks = Vec::new();
        for lane in lanes {
            // 關機開始了：已派出的照舊，剩下的不再開（shutdown 正在等這把 lifecycle 鎖）
            if self.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            if self.steering.lock().unwrap().contains(&lane) {
                continue;
            }
            // Durable admission survives parking/restart. Never interpret unreadable state as healthy.
            if !self.busy(&lane) {
                if let Some(c) = &self.continuity {
                    let error = match c.fault(&lane) {
                        Ok(e) => e,
                        Err(e) => Some(e.to_string()),
                    };
                    if let Some(error) = error {
                        self.notice_blocked(&lane, &error);
                        tasks.push(tokio::spawn(
                            async move { Err(aaf_core::Error::Other(error)) },
                        ));
                        continue;
                    }
                }
            }
            // A context fault blocks both new turns and steering for this lane.
            if let Some(error) = self
                .lanes
                .lock()
                .await
                .get(&lane)
                .and_then(|s| s.context_fault.lock().unwrap().clone())
            {
                self.notice_blocked(&lane, &error);
                let lane = lane.clone();
                tasks.push(tokio::spawn(async move {
                    Err(aaf_core::Error::Other(format!(
                        "context recovery required for {lane}: {error}"
                    )))
                }));
                continue;
            }
            if self.busy(&lane) {
                if let Some(t) = self.on_busy(&lane).await? {
                    tasks.push(t);
                }
                continue;
            }
            // 失敗後退避中：時間未到不重派
            if self
                .retries
                .lock()
                .unwrap()
                .get(&lane)
                .and_then(|s| s.not_before)
                .is_some_and(|t| Instant::now() < t)
            {
                continue;
            }
            if let Some(c) = &self.continuity {
                match c.rotate(&lane) {
                    Ok(true) => {
                        self.write_handoff(&lane, c.as_ref()).await;
                        if let Err(e) = self.close_locked(&lane, true).await {
                            error.get_or_insert(e);
                            continue;
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        error.get_or_insert(e);
                        continue;
                    }
                }
            }
            let Some(d) = self.driver_for(&lane).await? else {
                if self.config_failed.lock().unwrap().remove(&lane) {
                    let ids: Vec<i64> = self
                        .inbox
                        .unread(&lane)
                        .await?
                        .iter()
                        .map(|m| m.id)
                        .collect();
                    if !ids.is_empty() {
                        self.failure_ctx()
                            .fail(&lane, &ids, "agent 設定錯誤，無法啟動", false)
                            .await?;
                    }
                }
                continue;
            };
            if d.status() == AgentStatus::Offline {
                if let Err(e) = d.start().await {
                    tracing::warn!(%lane, error = %e, "driver start failed");
                    self.bus.publish(
                        "runtime.lane.start_failed",
                        1,
                        json!({"lane": lane_json(&lane), "error": e.to_string()}),
                    )?;
                    let ids: Vec<i64> = self
                        .inbox
                        .unread(&lane)
                        .await?
                        .iter()
                        .map(|m| m.id)
                        .collect();
                    if !ids.is_empty() {
                        self.failure_ctx()
                            .fail(&lane, &ids, &format!("agent 啟動失敗：{e}"), true)
                            .await?;
                    }
                    continue;
                }
                // Native-session state changes immediately after start succeeds,
                // before fallible persistence can return with an already-idle driver.
                if let Some(s) = self.lanes.lock().await.get_mut(&lane) {
                    let session_new = !d.session_loaded();
                    s.recovery_pending
                        .store(session_new, std::sync::atomic::Ordering::SeqCst);
                    if session_new {
                        s.context_fp = None;
                        s.turns = 0;
                    }
                }
                if let Some(c) = &self.continuity {
                    if let Err(e) = c.started(&lane, d.session_id().as_deref(), d.session_loaded())
                    {
                        if let Some(s) = self.lanes.lock().await.get(&lane) {
                            *s.context_fault.lock().unwrap() = Some(e.to_string());
                        }
                        error.get_or_insert(e);
                        continue;
                    }
                }
                if let Some(sid) = d.session_id() {
                    self.inbox.save_session(&lane, &sid).await?;
                }
            }
            if d.status() != AgentStatus::Idle {
                continue;
            }
            let msgs = self.inbox.unread(&lane).await?;
            if msgs.is_empty() {
                continue;
            }
            let recovery_pending = self.lanes.lock().await[&lane].recovery_pending.clone();
            let flush = self.lanes.lock().await[&lane].flush.clone();
            let recording_closed = self.lanes.lock().await[&lane].recording_closed.clone();
            let context_fault = self.lanes.lock().await[&lane].context_fault.clone();
            let sanitizer = self.lanes.lock().await[&lane].sanitizer.clone();
            let prepared = match &self.continuity {
                Some(c) => match c.prepare(
                    &lane,
                    recovery_pending.load(std::sync::atomic::Ordering::SeqCst),
                    &sanitizer.messages(&msgs),
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        error.get_or_insert(e);
                        continue;
                    }
                },
                None => Default::default(),
            };
            let continuity = self.continuity.clone();
            if let Some(s) = self.lanes.lock().await.get_mut(&lane) {
                s.last_active = Instant::now();
                // 新回合：前一輪的停止請求不延續到這一輪
                s.cancel_at = None;
                *s.last_event.lock().unwrap() = Instant::now();
            }
            woke.push(lane.clone());
            self.in_flight.lock().unwrap().insert(lane.clone());
            let (inbox, bus, in_flight) =
                (self.inbox.clone(), self.bus.clone(), self.in_flight.clone());
            let force_closed = self.force_closed.clone();
            let roles: std::collections::BTreeSet<String> =
                self.roster().roles.keys().cloned().collect();
            force_closed.lock().unwrap().remove(&lane); // 清掉前一輪殘留的標記
            let mut preamble = self.preamble.as_ref().map(|f| f(&lane)).unwrap_or_default();
            preamble = format!("{}{preamble}", prepared.text);
            if let Some((fp, text)) = self.context.as_ref().and_then(|f| f(&lane)) {
                let mut lanes = self.lanes.lock().await;
                if let Some(s) = lanes.get_mut(&lane) {
                    if s.context_fp != Some(fp) {
                        s.context_fp = Some(fp);
                        preamble = format!("{text}{preamble}");
                        let _ = self.bus.publish(
                            "runtime.context.sent",
                            1,
                            json!({"lane": lane_json(&lane), "fingerprint": fp.to_string()}),
                        );
                    }
                }
            }
            let session_new = {
                let mut lanes = self.lanes.lock().await;
                let s = lanes.get_mut(&lane);
                let first = s.as_ref().is_some_and(|s| s.turns == 0);
                if let Some(s) = s {
                    s.turns += 1;
                }
                first
            };
            let fail = self.failure_ctx();
            let is_hermes = self.is_hermes_lane(&lane);
            let hooks = self.hooks.clone();
            if let Some(h) = &hooks {
                let ev = json!({"type": "wake", "role": lane.role, "room": lane.room, "unread": msgs.len(), "session_new": session_new});
                preamble = format!("{}{preamble}", h.wake(&ev).await);
            }
            self.turn_meta.lock().unwrap().insert(
                lane.clone(),
                TurnMeta {
                    batch: msgs.iter().map(|m| m.id).collect(),
                    started: Some(Instant::now()),
                    ..Default::default()
                },
            );
            let meta_guard = TurnMetaGuard(self.turn_meta.clone(), lane.clone());
            tasks.push(tokio::spawn(async move {
                let _guard = InFlight(in_flight, lane.clone());
                let _meta = meta_guard;
                let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
                bus.publish(
                    "runtime.turn.started",
                    1,
                    json!({"lane": lane_json(&lane), "messages": ids}),
                )?;
                let mut turn_err: Option<String> = None;
                let mut turn_text = String::new();
                if continuity.is_some() {
                    crate::continuity::flush_events(&flush, &recording_closed).await?;
                    if let Some(error) = context_fault.lock().unwrap().clone() {
                        return Err(aaf_core::Error::Other(error));
                    }
                }
                let turn_started = crate::ratelimit::now_secs();
                let result = d
                    .prompt(&format!("{preamble}{}", compose_prompt(&lane, &msgs)))
                    .await;
                // 這一輪收到新的額度視窗就記一筆事件（額度頁讀最新一筆；不多花 token）
                if let Some(rl) = d.rate_limit() {
                    if rl["observed_at"].as_f64().is_some_and(|t| t >= turn_started) {
                        let _ = bus.publish("runtime.agent.ratelimit", 1, rl);
                    }
                }
                if let Some(c) = &continuity {
                    let committed: aaf_core::Result<()> = async {
                        crate::continuity::flush_events(&flush,&recording_closed).await?;
                        c.complete(&lane, &prepared, &sanitizer.result(&result))?;
                        match d.context_usage() {
                            Some(u) => c.usage(&lane, &u),
                            None => Ok(()),
                        }
                    }.await;
                    if let Err(e) = committed {
                        *context_fault.lock().unwrap() = Some(e.to_string());
                        let _ = c.failed(&lane, &e.to_string());
                        let _ = bus.publish("runtime.context.failed",1,json!({"lane":lane_json(&lane),"error":e.to_string(),"recovery":"repair storage and close the lane; read back unknown external effects"}));
                        return Err(e);
                    }
                }
                if result.as_ref().is_ok_and(|r| r.stop_reason == "end_turn" && failure_copy(&r.text).is_none()) {
                    recovery_pending.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                // 服務關機中斷（不論 driver 回 Err 或 Ok(cancelled)）：不 ack、不公告、不重試，訊息留在佇列
                let shut = !result.as_ref().is_ok_and(|r| r.stop_reason == "end_turn")
                    && force_closed.lock().unwrap().get(&lane) == Some(&"shutdown");
                match result {
                    _ if shut => {
                        force_closed.lock().unwrap().remove(&lane);
                        fail.retries.lock().unwrap().remove(&lane);
                        bus.publish(
                            "runtime.turn.incomplete",
                            1,
                            json!({"lane": lane_json(&lane), "stop_reason": "shutdown", "messages": ids}),
                        )?;
                    }
                    Ok(r) if r.stop_reason == "end_turn"
                        && ((is_hermes && hermes_failure_copy(&r.text).is_some())
                            || codex_failure_copy(&r.text).is_some()) =>
                    {
                        // 引擎把錯誤說明當成回覆（Hermes 用完重試、Codex 連線失敗）：不以角色身分貼出，走失敗流程
                        let retry = failure_copy(&r.text).unwrap_or(false);
                        turn_err = Some(first_line(&r.text));
                        fail.fail(&lane, &ids, &first_line(&r.text), retry).await?;
                    }
                    Ok(r) if r.stop_reason == "end_turn" => {
                        fail.retries.lock().unwrap().remove(&lane);
                        turn_text = r.text.clone();
                        let body = r.text.trim();
                        // Hermes 重試後仍空回覆時以「(empty)」當最終文字；視同沒有回覆（不貼群），留事件紀錄
                        if body == EMPTY_REPLY_MARKER {
                            bus.publish(
                                "runtime.turn.empty_reply",
                                1,
                                json!({"lane": lane_json(&lane), "messages": ids}),
                            )?;
                        } else if !body.is_empty() {
                            // 回覆最後一封非自己寄的信：沿用其群與討論串
                            let anchor = msgs
                                .iter()
                                .rev()
                                .find(|m| m.from != lane.role)
                                .unwrap_or(msgs.last().expect("non-empty"));
                            inbox.reply(&lane.role, anchor, r.text.trim()).await?;
                        }
                        inbox.ack(&lane.role, &ids).await?;
                    }
                    Ok(r) if r.stop_reason == "cancelled" => {
                        // 使用者按停止：不自動重跑，但公告沒處理完的訊息（不可無聲消失）
                        fail.retries.lock().unwrap().remove(&lane);
                        fail.stopped(&lane, &msgs, &roles).await?;
                        bus.publish(
                            "runtime.turn.incomplete",
                            1,
                            json!({"lane": lane_json(&lane), "stop_reason": r.stop_reason, "messages": ids}),
                        )?;
                    }
                    Ok(r) => {
                        // max_tokens／refusal／error／command 角色非零結束……：不可無聲消失
                        bus.publish(
                            "runtime.turn.incomplete",
                            1,
                            json!({"lane": lane_json(&lane), "stop_reason": r.stop_reason}),
                        )?;
                        let (reason, retry) = match r.stop_reason.as_str() {
                            "max_tokens" | "max_turn_requests" => {
                                // 已產生的部分照貼，不重試（重跑多半同樣被截）
                                let part = r.text.trim();
                                if !part.is_empty() && part != EMPTY_REPLY_MARKER {
                                    let anchor = msgs
                                        .iter()
                                        .rev()
                                        .find(|m| m.from != lane.role)
                                        .unwrap_or(msgs.last().expect("non-empty"));
                                    inbox.reply(&lane.role, anchor, part).await?;
                                }
                                ("回覆超過長度上限，內容不完整".to_string(), false)
                            }
                            "refusal" => ("模型拒絕回答".to_string(), false),
                            other => (format!("agent 回報 {other}"), true),
                        };
                        turn_err = Some(reason.clone());
                        fail.fail(&lane, &ids, &reason, retry).await?;
                    }
                    Err(_) if force_closed.lock().unwrap().contains_key(&lane) => {
                        fail.retries.lock().unwrap().remove(&lane);
                        let reason = force_closed.lock().unwrap().remove(&lane).unwrap_or_default();
                        // 卡住被強制關閉：視為已處理，避免重派同一批又卡住（重派會每次都卡）。
                        // 使用者按的停止：同取消，安靜結束。
                        // 看門狗（沒人按停止）：在群裡留一則可見的中斷通知，列出沒做完的訊息。
                        if reason == "no_events" {
                            let list = ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join("、");
                            let text = format!(
                                "⚠ 系統：{} 這一輪長時間沒有任何進度，已強制中斷；{list} 沒有做完，需要的話請重新交代。",
                                lane.role
                            );
                            // 公告確實寫入才標成已處理；沒寫進去就留在佇列（不能無聲消失）
                            match inbox.notify(lane.room, &text).await {
                                Ok(Some(_)) => inbox.ack(&lane.role, &ids).await?,
                                other => {
                                    bus.publish(
                                        "runtime.lane.notice_failed",
                                        1,
                                        json!({"lane": lane_json(&lane), "messages": ids, "error": other.err().map(|e| e.to_string())}),
                                    )?;
                                }
                            }
                        } else {
                            // 按了停止、agent 不理會而被強制關閉：同取消
                            fail.stopped(&lane, &msgs, &roles).await?;
                        }
                        bus.publish(
                            "runtime.turn.incomplete",
                            1,
                            json!({"lane": lane_json(&lane), "stop_reason": "force_closed", "reason": reason, "messages": ids}),
                        )?;
                    }
                    Err(e) => {
                        turn_err = Some(e.to_string());
                        fail.fail(&lane, &ids, &format!("agent 錯誤：{e}"), true).await?;
                    }
                }
                if let Some(h) = &hooks {
                    let ev = json!({"type": "turn_end", "role": lane.role, "room": lane.room, "ok": turn_err.is_none(), "error": turn_err, "text": turn_text.chars().rev().take(2000).collect::<String>().chars().rev().collect::<String>()});
                    h.turn_end(&ev).await;
                }
                Ok::<_, aaf_core::Error>(())
            }));
        }
        Ok((woke, tasks, error))
    }
}

/// 關機時 agent 停掉後，等回合 task 收尾的上限。
const SHUTDOWN_TURN_DRAIN: Duration = Duration::from_secs(10);

/// Hermes 的空回覆標記（`agent/turn_empty_response.py` `_terminal_empty`）：不當成回覆貼到群裡。
const EMPTY_REPLY_MARKER: &str = "(empty)";

/// 回合結束（含 panic）時移出 in-flight。
struct InFlight(Arc<std::sync::Mutex<std::collections::HashSet<Lane>>>, Lane);

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut s) = self.0.lock() {
            s.remove(&self.1);
        }
    }
}
