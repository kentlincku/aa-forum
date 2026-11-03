//! 群聊功能模組：群、成員、訊息、垃圾桶與永久刪除，並以 SQLite 信箱接上 dispatcher。
//!
//! 投遞規則（避免 agent 互相無限回覆）：
//! - 人發的訊息：有 @提及角色就只給被提及的角色，否則給群內所有角色。
//! - 角色發的訊息（含 dispatcher 代寄的回覆）：只給被 @提及的其他角色。
//! - 凍結、封存、刪除的群不喚醒任何角色。

pub mod approval;
pub mod attachments;
mod http;
pub mod inbox;
mod schema;
pub mod search;
pub mod store;
pub mod tasks;
pub mod transcript;

use aaf_core::config::ConfigKey;
use aaf_core::jobs::Job;
use aaf_core::perms::{CapabilityDef, Rank};
use aaf_core::storage::Migration;
use aaf_core::Core;
use aaf_runtime::dispatcher::{shared_roster, Dispatcher, Roster, SharedRoster};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const CAPS: &[CapabilityDef] = &[
    CapabilityDef {
        id: "chat.room.create",
        description: "建立群",
        default_ranks: &[Rank::Human],
    },
    CapabilityDef {
        id: "chat.room.manage",
        description: "改名、資料夾、釘選、凍結、邀請／移除成員",
        default_ranks: &[Rank::Human],
    },
    CapabilityDef {
        id: "chat.room.archive",
        description: "封存、刪除（移到垃圾桶）、恢復群",
        default_ranks: &[],
    },
    CapabilityDef {
        id: "chat.room.purge",
        description: "永久刪除群",
        default_ranks: &[],
    },
    CapabilityDef {
        id: "chat.message.post",
        description: "發訊息",
        default_ranks: &[Rank::Human, Rank::Lead, Rank::Worker],
    },
    CapabilityDef {
        id: "chat.task.post",
        description: "建立與指派任務",
        default_ranks: &[Rank::Human, Rank::Lead],
    },
    CapabilityDef {
        id: "chat.task.work",
        description: "認領、回報任務",
        default_ranks: &[Rank::Human, Rank::Lead, Rank::Worker],
    },
    CapabilityDef {
        id: "chat.agent.control",
        description: "停止回合、關閉 session、回應權限請求",
        default_ranks: &[Rank::Human],
    },
];

pub const CONFIG: &[ConfigKey] = &[
    ConfigKey {
        key: "AAF_CHAT_TRASH_RETENTION_DAYS",
        default: "30",
        description: "垃圾桶保留天數，超過自動永久刪除；0＝不自動清空",
    },
    ConfigKey {
        key: "AAF_CHAT_FILE_MB",
        default: "100",
        description: "群聊附檔單檔上限（MB）；0＝不限。圖片固定 5 MB",
    },
    ConfigKey {
        key: "AAF_TRANSCRIPT_MINUTES",
        default: "120",
        description: "逐字稿自動匯出間隔（分鐘）；0＝停用",
    },
    ConfigKey {
        key: "AAF_TRANSCRIPT_DIR",
        default: "",
        description: "逐字稿輸出目錄；空白＝<instance>/outputs/transcripts",
    },
    ConfigKey {
        key: "AAF_STALL_MINUTES",
        default: "30",
        description: "任務超過幾分鐘沒有更新算停住（停滯清單）；0＝關閉",
    },
    ConfigKey {
        key: "AAF_LANE_STALL_MINUTES",
        default: "60",
        description:
            "agent 忙碌中超過幾分鐘沒有任何事件就視為卡死、強制關閉該 lane（保留 session）；0＝關閉",
    },
    ConfigKey {
        key: "AAF_CHAT_AGENT_CHAIN",
        default: "8",
        description: "同群自上一則人類發言起，角色連發超過幾則就不再互相叫醒（防 agent 互相 @ 停不下來），有人發言後恢復；0＝不限制",
    },
    ConfigKey {
        key: "AAF_LEAD_SEES_ALL",
        default: "1",
        description:
            "群裡每則訊息（包括角色沒 @ 人的發言、人 @ 別人的訊息）都送給該群的 lead，讓帶隊掌握全群；0＝只有被 @ 或人沒 @ 時才收到",
    },
    ConfigKey {
        key: "AAF_STEER",
        default: "1",
        description:
            "agent 回合進行中收到新信時，用 Hermes /steer 即時插入（不必等這輪結束）；0＝照舊排隊",
    },
    ConfigKey {
        key: "AAF_BUSY_NOTICE_MINUTES",
        default: "10",
        description:
            "收件 agent 這一輪已忙超過幾分鐘、新信又插不進去時，在群裡公告「忙碌中、已排隊」（每封信一次）；0＝關閉",
    },
    ConfigKey {
        key: "AAF_STOP_NOTICE",
        default: "1",
        description:
            "按停止中斷 agent 回合時，在群裡公告沒處理完的訊息並 @ 寄信的角色請其重送；0＝安靜結束（舊行為）",
    },
    ConfigKey {
        key: "AAF_TURN_RETRY",
        default: "2",
        description:
            "agent 回合失敗（連線中斷、錯誤、Hermes 錯誤回覆）時自動重派幾次（間隔 30 秒、120 秒），用完在群裡公告；0＝不重試、直接公告",
    },
    ConfigKey {
        key: "AAF_TZ",
        default: "",
        description: "逐字稿時區（IANA 名稱，例如 Asia/Taipei）；空白＝系統時區",
    },
];

/// 每輪提示前言：說明身分與可用指令（agent 以 AAF_TOKEN 呼叫 API）。
fn preamble(lane: &aaf_runtime::dispatcher::Lane, members: Option<String>) -> String {
    let room = lane
        .room
        .map(|r| r.to_string())
        .unwrap_or_else(|| "-".into());
    format!(
        "[AA Forum] You are role `{role}` (room {room}). Your reply text is posted to the room automatically — just answer; do not also post it with `aaf chat post`.\n\
         Tools (run in a shell; AAF_TOKEN/AAF_URL/AAF_ROOM are already set):\n\
         - `aaf task list` · `aaf task claim <id>` · `aaf task done <id> \"result\"` · `aaf task blocked <id> \"reason\"`\n\
         - `aaf task new \"title\" [--spec ...] [--to <role>]` (lead only)\n\
         - `aaf chat post \"text\" [--file path]...` only for EXTRA posts (e.g. with files, or a second message); never use it to repeat your reply, which is posted for you (mention @role to wake another agent)\n\
         Attachments in messages are listed with a read-only local path you can open.\n{members}\n",
        role = lane.role,
        members = members.map(|m| format!("{m}\n")).unwrap_or_default()
    )
}

/// 前言的成員行：讓派工的人知道 @ 誰才收得到。
pub(crate) fn members_line(c: &rusqlite::Connection, room: i64) -> Option<String> {
    let all = store::members(c, room).ok()?;
    let pick = |k: &str| {
        let v: Vec<&str> = all
            .iter()
            .filter(|m| m.kind == k)
            .map(|m| m.member.as_str())
            .collect();
        if v.is_empty() {
            "-".to_string()
        } else {
            v.join(", ")
        }
    };
    Some(format!(
        "Room members — roles: {}; people: {}. Only these receive @mentions; anyone else is not delivered — pick from this list or ask the owner to invite.",
        pick("role"),
        pick("user")
    ))
}

/// 群聊模組。持有角色設定；dispatcher 在核心啟動後才建立（需要事件匯流排與資料庫）。
pub struct Chat {
    pub(crate) shared: Arc<Shared>,
}

type ContinuityFactory =
    Arc<dyn Fn(&Core) -> Arc<dyn aaf_runtime::continuity::Continuity> + Send + Sync>;

pub struct Shared {
    continuity: std::sync::RwLock<Option<ContinuityFactory>>,
    roster: SharedRoster,
    /// 角色 token（只在記憶體）：啟動 agent 時以 AAF_TOKEN 注入。每次啟動重新發，舊的隨之失效。
    tokens: Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>,
    /// agent 呼叫 API 的位址（例如 http://127.0.0.1:8111）
    api_url: std::sync::RwLock<String>,
    dispatcher: OnceLock<Arc<Dispatcher>>,
    /// dispatcher 建立時記下的 DB（context provider 查群級 lead 用）
    db: OnceLock<aaf_core::storage::Db>,
    tick: Duration,
    /// roles.json 上次載入時的修改時間與內容雜湊（熱重載判斷用）
    seen: std::sync::Mutex<Option<(std::time::SystemTime, u64)>>,
    /// 其他 feature 提供的長內容（例如 skills）；在 dispatcher 建立前設定
    context: std::sync::RwLock<Option<aaf_runtime::dispatcher::ContextProvider>>,
    /// 外部規則（hooks）；None＝不啟用
    pub(crate) hooks: std::sync::RwLock<Option<aaf_runtime::hooks::Hooks>>,
    /// 服務關機中：派工 job 不再動作（dispatcher 尚未建立時也不會再建立來派工）
    stopping: std::sync::atomic::AtomicBool,
}

impl Shared {
    /// 發文／派工前檢查規則；回 Some(原因) 表示擋下。人與角色都適用，系統訊息不跑。
    pub async fn hook_pre(&self, ev: &serde_json::Value) -> Option<String> {
        let h = self.hooks.read().unwrap().clone()?;
        h.pre(ev).await
    }

    pub async fn hook_post(&self, ev: &serde_json::Value) {
        let h = self.hooks.read().unwrap().clone();
        if let Some(h) = h {
            h.post(ev).await;
        }
    }

    /// 依目前 roster 發／撤角色 token：新角色發新 token、移除的角色撤銷、rank 改變則換發。
    pub fn sync_tokens(&self, core: &Core) -> aaf_core::Result<()> {
        let roster = self.roster();
        let mut map = self.tokens.write().unwrap();
        let have: Vec<String> = map.keys().cloned().collect();
        for r in have {
            if !roster.roles.contains_key(&r) {
                aaf_api::revoke_role_token(core, &r)?;
                map.remove(&r);
            }
        }
        for (name, spec) in &roster.roles {
            if spec.driver.is_manual() {
                continue;
            }
            let current_rank: Option<String> = core.db.with(|c| {
                use rusqlite::OptionalExtension;
                Ok(c.query_row(
                    "SELECT rank FROM auth_role_tokens WHERE role = ?1",
                    [name],
                    |r| r.get(0),
                )
                .optional()?)
            })?;
            if !map.contains_key(name) || current_rank.as_deref() != Some(spec.rank.as_str()) {
                map.insert(
                    name.clone(),
                    aaf_api::issue_role_token(core, name, &spec.rank)?,
                );
            }
        }
        Ok(())
    }

    /// 逐字稿輸出根目錄。
    pub fn transcript_root(&self, core: &Core) -> std::path::PathBuf {
        transcript::root(core, &self.roster().instance)
    }

    /// 立即匯出逐字稿。
    pub fn export_transcripts(
        &self,
        core: &Core,
        rooms: Option<&[i64]>,
        since: Option<&str>,
    ) -> aaf_core::Result<transcript::Report> {
        let root = self.transcript_root(core);
        let tz = core.config.get("AAF_TZ").ok().map(str::to_string);
        let rep = transcript::export(
            core,
            &transcript::Options {
                root: &root,
                tz: tz.as_deref(),
                rooms,
                since,
            },
        )?;
        if !rep.written.is_empty() {
            core.bus.publish(
                "chat.transcript.exported",
                1,
                serde_json::json!({"files": rep.written.len(), "rooms": rep.rooms}),
            )?;
        }
        Ok(rep)
    }

    /// 目前生效的角色設定快照。
    pub fn roster(&self) -> Arc<Roster> {
        self.roster.read().unwrap().clone()
    }

    /// 重新讀 roles.json 並套用。檔案壞掉時保留舊設定並回錯誤（不會讓團隊停擺）。
    pub async fn reload(
        &self,
        core: &Core,
    ) -> aaf_core::Result<aaf_runtime::dispatcher::RosterDiff> {
        let cur = self.roster();
        let Some(path) = cur.source.clone() else {
            return Ok(Default::default());
        };
        let home = cur.instance.clone();
        match Roster::load(&home) {
            Ok(new) => {
                let d = self.dispatcher(core).apply_roster(new).await?;
                self.sync_tokens(core)?;
                Ok(d)
            }
            Err(e) => {
                core.bus.publish(
                    "runtime.roster.invalid",
                    1,
                    serde_json::json!({"path": path, "error": e.to_string()}),
                )?;
                Err(e)
            }
        }
    }

    /// 檔案有變（修改時間或內容）才重載；第一次只記錄現況。
    async fn reload_if_changed(&self, core: &Core) -> aaf_core::Result<()> {
        let Some(path) = self.roster().source.clone() else {
            return Ok(());
        };
        let stamp = match std::fs::read(&path) {
            Ok(b) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                b.hash(&mut h);
                Some((
                    std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH),
                    h.finish(),
                ))
            }
            Err(_) => None,
        };
        let prev = *self.seen.lock().unwrap();
        if stamp == prev {
            return Ok(());
        }
        let first = prev.is_none() && self.dispatcher.get().is_none();
        *self.seen.lock().unwrap() = stamp;
        if first {
            return Ok(());
        }
        match self.reload(core).await {
            Ok(d) if !d.is_empty() => tracing::info!(?d, "roles.json reloaded"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "roles.json invalid; keeping previous roster"),
        }
        Ok(())
    }

    /// 服務關機：停止派工並停掉所有 agent（見 [`Dispatcher::shutdown`]）。回傳停不掉的 lane 與原因。
    pub async fn shutdown(&self) -> Vec<(aaf_runtime::dispatcher::Lane, String)> {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        match self.dispatcher.get() {
            Some(d) => d.shutdown().await,
            None => Vec::new(),
        }
    }

    /// 本群指定的 lead（只在該群帶 lead 包）。dispatcher 還沒建立時回 None。
    pub fn room_lead(&self, room: i64) -> Option<String> {
        self.db
            .get()?
            .with(|c| store::room_lead(c, room))
            .ok()
            .flatten()
    }

    /// 取得 dispatcher（第一次呼叫時以核心的 db／bus 建立）。
    pub fn dispatcher(&self, core: &Core) -> Arc<Dispatcher> {
        let _ = self.db.set(core.db.clone());
        if self.dispatcher.get().is_none() {
            if let Err(e) = self.sync_tokens(core) {
                tracing::error!(error = %e, "issuing role tokens failed");
            }
        }
        self.dispatcher
            .get_or_init(|| {
                let inbox = Arc::new(inbox::ChatInbox {
                    db: core.db.clone(),
                    bus: core.bus.clone(),
                    roster: self.roster.clone(),
                    hooks: self.hooks.read().unwrap().clone(),
                });
                let tokens = self.tokens.clone();
                let url = self.api_url.read().unwrap().clone();
                Arc::new(
                    Dispatcher::with_shared(inbox, core.bus.clone(), self.roster.clone())
                        .with_continuity(self.continuity.read().unwrap().as_ref().map(|f| f(core)))
                        .with_stall_kill(Duration::from_secs(
                            60 * core
                                .config
                                .parse::<u64>("AAF_LANE_STALL_MINUTES")
                                .unwrap_or(60),
                        ))
                        .with_turn_retry(core.config.parse::<u32>("AAF_TURN_RETRY").unwrap_or(2))
                        .with_steer(core.config.parse::<u32>("AAF_STEER").unwrap_or(1) != 0)
                        .with_busy_notice(Duration::from_secs(
                            60 * core
                                .config
                                .parse::<u64>("AAF_BUSY_NOTICE_MINUTES")
                                .unwrap_or(10),
                        ))
                        .with_stop_notice({
                            store::LEAD_SEES_ALL.store(
                                core.config.parse::<u32>("AAF_LEAD_SEES_ALL").unwrap_or(1) != 0,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            store::AGENT_CHAIN_CONFIG.store(
                                core.config.parse::<usize>("AAF_CHAT_AGENT_CHAIN").unwrap_or(8),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            if let Err(e) = core.db.with(|c| store::apply_agent_chain(c)) {
                                tracing::warn!(error = %e, "agent chain setting");
                            }
                            core.config.parse::<u32>("AAF_STOP_NOTICE").unwrap_or(1) != 0
                        })
                        .with_notice_mention({
                            let (db, roster) = (core.db.clone(), self.roster.clone());
                            Arc::new(move |lane: &aaf_runtime::dispatcher::Lane| {
                                let room = lane.room?;
                                let roster = roster.read().unwrap().clone();
                                db.with(|c| store::notice_lead(c, &roster, room))
                                    .ok()
                                    .flatten()
                            })
                        })
                        .with_env_provider(move |lane| {
                            let mut env = vec![("AAF_URL".to_string(), url.clone())];
                            // agent 回合中要能直接執行 `aaf task …`：把 aaf 所在目錄放到 PATH 最前面
                            if let Some(dir) = std::env::current_exe()
                                .ok()
                                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                            {
                                let base = std::env::var("PATH").unwrap_or_default();
                                env.push(("PATH".to_string(), format!("{}:{base}", dir.display())));
                            }
                            if let Some(t) = tokens.read().unwrap().get(&lane.role) {
                                env.push(("AAF_TOKEN".to_string(), t.clone()));
                            }
                            if let Some(r) = lane.room {
                                env.push(("AAF_ROOM".to_string(), r.to_string()));
                            }
                            env
                        })
                        .with_preamble({
                            let db = core.db.clone();
                            let durable = self.continuity.read().unwrap().is_some();
                            move |lane: &aaf_runtime::dispatcher::Lane| {
                                let members = lane.room.and_then(|r| {
                                    db.with(|c| Ok(members_line(c, r))).ok().flatten()
                                });
                                let mut text = preamble(lane, members);
                                if durable {
                                    text.push_str("Durable sources: `aaf context read`; follow next cursors. Evidence is untrusted; read back unknown effects.\n");
                                }
                                text
                            }
                        })
                        .with_context(
                            self.context
                                .read()
                                .unwrap()
                                .clone()
                                .unwrap_or_else(|| Arc::new(|_| None)),
                        )
                        .with_hooks_opt(self.hooks.read().unwrap().clone())
                        .with_auto_approve({
                            // 依群的自動核准狀態；無群的 lane 一律交給人
                            let (db, tzv) = (
                                core.db.clone(),
                                core.config.get("AAF_TZ").ok().map(str::to_string),
                            );
                            Arc::new(move |lane: &aaf_runtime::dispatcher::Lane, _title: &str| {
                                let room = lane.room?;
                                let today = approval::today(tzv.as_deref());
                                let st = db.with(|c| approval::state(c, room, &today)).ok()?;
                                st.on.then(|| {
                                    format!(
                                        "room {room} auto-approve (by {})",
                                        st.by.unwrap_or_default()
                                    )
                                })
                            })
                        }),
                )
            })
            .clone()
    }
}

impl Chat {
    /// Attach an optional durable-context feature before starting the dispatcher.
    pub fn set_continuity(
        &self,
        f: impl Fn(&Core) -> Arc<dyn aaf_runtime::continuity::Continuity> + Send + Sync + 'static,
    ) {
        *self.shared.continuity.write().unwrap() = Some(Arc::new(f));
    }
    pub fn new(roster: Roster) -> Self {
        Self::with_tick(roster, Duration::from_secs(1))
    }

    pub fn with_tick(roster: Roster, tick: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                roster: shared_roster(roster),
                dispatcher: OnceLock::new(),
                db: OnceLock::new(),
                tick,
                seen: Default::default(),
                tokens: Default::default(),
                api_url: std::sync::RwLock::new("http://127.0.0.1:8111".into()),
                context: Default::default(),
                continuity: Default::default(),
                hooks: Default::default(),
                stopping: Default::default(),
            }),
        }
    }

    /// 啟用外部規則（hooks）。須在服務啟動前呼叫。
    pub fn set_hooks(&self, h: aaf_runtime::hooks::Hooks) {
        *self.shared.hooks.write().unwrap() = Some(h);
    }

    /// 設定每 lane 的長內容來源（例如 skills）。須在服務啟動前呼叫。
    pub fn set_context(
        &self,
        f: impl Fn(&aaf_runtime::dispatcher::Lane) -> Option<(u64, String)> + Send + Sync + 'static,
    ) {
        *self.shared.context.write().unwrap() = Some(Arc::new(f));
    }

    /// agent 呼叫 API 的位址（aaf up 依 --listen 設定）。
    pub fn set_api_url(&self, url: &str) {
        *self.shared.api_url.write().unwrap() = url.trim_end_matches('/').to_string();
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }
}

impl aaf_core::Feature for Chat {
    fn name(&self) -> &'static str {
        "chat"
    }
    fn migrations(&self) -> &'static [Migration] {
        schema::MIGRATIONS
    }
    fn capabilities(&self) -> &'static [CapabilityDef] {
        CAPS
    }
    fn config(&self) -> &'static [ConfigKey] {
        CONFIG
    }
    fn jobs(&self) -> Vec<Job> {
        let (a, b, c) = (
            self.shared.clone(),
            self.shared.clone(),
            self.shared.clone(),
        );
        vec![
            // 派工：不等回合結束，慢的 lane 不擋其他 lane
            Job::every("dispatch", self.shared.tick, move |core| {
                let s = a.clone();
                async move {
                    if s.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                        return Ok(());
                    }
                    let d = s.dispatcher(&core);
                    // 建立 dispatcher 時剛好開始關機（shutdown 沒看到它）：再查一次
                    if s.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                        return Ok(());
                    }
                    // Maintenance must run even when one lane cannot dispatch.
                    let dispatch = d.tick().await;
                    let park = d.park_idle().await;
                    let reap = d.reap_stuck().await;
                    dispatch.and(park).and(reap).map(|_| ())
                }
            }),
            // roles.json 熱重載：每 2 秒檢查一次修改時間與內容
            Job::every("roster_watch", Duration::from_secs(2), move |core| {
                let s = c.clone();
                async move { s.reload_if_changed(&core).await }
            }),
            // 逐字稿：每分鐘檢查，依 AAF_TRANSCRIPT_MINUTES 決定是否到期（設定可熱改）
            Job::every("transcript_export", Duration::from_secs(60), {
                let s = self.shared.clone();
                let last = Arc::new(std::sync::Mutex::new(0i64));
                move |core| {
                    let (s, last) = (s.clone(), last.clone());
                    async move {
                        let mins: i64 = core.config.parse("AAF_TRANSCRIPT_MINUTES")?;
                        // 沒有 instance 目錄（測試用的空 roster）且沒指定輸出目錄：不寫到工作目錄
                        let explicit = core
                            .config
                            .get("AAF_TRANSCRIPT_DIR")
                            .is_ok_and(|d| !d.trim().is_empty());
                        if s.roster().instance.as_os_str().is_empty() && !explicit {
                            return Ok(());
                        }
                        let now = store::now();
                        {
                            let mut l = last.lock().unwrap();
                            if mins <= 0 || now - *l < mins * 60 {
                                return Ok(());
                            }
                            *l = now;
                        }
                        let rep = tokio::task::spawn_blocking(move || {
                            s.export_transcripts(&core, None, None)
                        })
                        .await
                        .map_err(|e| aaf_core::Error::Other(e.to_string()))??;
                        tracing::info!(
                            rooms = rep.rooms,
                            files = rep.written.len(),
                            "transcripts exported"
                        );
                        Ok(())
                    }
                }
            }),
            Job::every("attachment_gc", Duration::from_secs(3600), {
                let s = self.shared.clone();
                move |core| {
                    let s = s.clone();
                    async move {
                        let inst = s.roster().instance.clone();
                        let n = tokio::task::spawn_blocking(move || attachments::gc(&core, &inst))
                            .await
                            .map_err(|e| aaf_core::Error::Other(e.to_string()))??;
                        if n > 0 {
                            tracing::info!(removed = n, "attachment blobs removed");
                        }
                        Ok(())
                    }
                }
            }),
            Job::every("task_leases", Duration::from_secs(30), |core| async move {
                tasks::expire_all(&core)?;
                Ok(())
            }),
            Job::every("trash_retention", Duration::from_secs(3600), move |core| {
                let s = b.clone();
                async move {
                    store::purge_expired(&core, &s.dispatcher(&core)).await?;
                    Ok(())
                }
            }),
        ]
    }
}

impl aaf_api::HttpFeature for Chat {
    fn name(&self) -> &'static str {
        "chat"
    }
    fn routes(&self) -> axum::Router<aaf_api::AppState> {
        http::routes(self.shared.clone())
    }
    fn nav(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("Rooms", "/rooms"),
            ("Search", "/search"),
            ("Tasks", "/tasks"),
            ("Trash", "/trash"),
        ]
    }

    /// chat.* 與 runtime.* 事件只送給該群成員；Owner 全看。
    fn can_see(
        &self,
        s: &aaf_api::AppState,
        u: &aaf_api::User,
        ev: &aaf_contract::EventEnvelope,
    ) -> bool {
        // Raw tool evidence is retrieved through the role-scoped context API, never room SSE.
        if ev.kind == "runtime.agent.tool_evidence" {
            return false;
        }
        if u.rank == aaf_core::perms::Rank::Owner {
            return true;
        }
        let room = if ev.kind.starts_with("runtime.") {
            ev.payload["lane"]["room"].as_i64()
        } else if ev.kind.starts_with("chat.") {
            ev.payload["message"]["room"]
                .as_i64()
                .or_else(|| ev.payload["task"]["room"].as_i64())
                .or_else(|| ev.payload["room"]["id"].as_i64())
                .or_else(|| ev.payload["room"].as_i64())
        } else {
            return true;
        };
        match room {
            Some(r) => s
                .core
                .db
                .with(|c| store::is_member(c, r, &u.name))
                .unwrap_or(false),
            // 找不到群的 chat.*／runtime.* 事件一律不送（fail closed），避免新事件漏過濾
            None => false,
        }
    }
}

#[cfg(test)]
mod members_line_tests {
    use super::*;

    #[test]
    fn lists_roles_and_people_of_the_room() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE chat_members(room INTEGER, member TEXT, kind TEXT, added_at INTEGER);
             INSERT INTO chat_members VALUES (1,'maker','role',0),(1,'planner','role',0),(1,'alice','user',0),(2,'auditor','role',0);",
        )
        .unwrap();
        let line = members_line(&c, 1).unwrap();
        assert!(line.contains("roles: maker, planner;"), "{line}");
        assert!(line.contains("people: alice."), "{line}");
        assert!(!line.contains("auditor"), "{line}");
        let lane = aaf_runtime::dispatcher::Lane::new("planner", Some(1));
        assert!(preamble(&lane, Some(line)).contains("Only these receive @mentions"));
        assert!(!preamble(&lane, None).contains("Room members"));
    }
}
