mod client;
mod doctor;
mod install;
mod service;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "aaf", version, about = "AA Forum")]
struct Cli {
    /// instance 目錄（資料庫、.aaf.env）；預設 $AAF_HOME 或目前目錄
    #[arg(long, env = "AAF_HOME", default_value = ".")]
    home: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read durable role-room context through the authenticated API
    Context {
        #[command(subcommand)]
        cmd: client::ContextCmd,
    },
    /// 在前景啟動服務（Ctrl-C 或 SIGTERM 停止）
    Up {
        #[arg(long, default_value = service::DEFAULT_LISTEN)]
        listen: String,
        /// `aaf start` 用：脫離呼叫端的工作階段（setsid）
        #[arg(long, hide = true)]
        detach: bool,
    },
    /// 在背景啟動服務（log 附加到 var/aaf-up.log），等到就緒才回傳；已在執行就不動作
    Start {
        /// 服務位址；預設沿用上次（var/aaf.run.json），沒有就是 127.0.0.1:8111
        #[arg(long)]
        listen: Option<String>,
        /// 最多等多久就緒（例如 30s、2m）
        #[arg(long, default_value = "30s", value_parser = service::parse_duration)]
        timeout: std::time::Duration,
    },
    /// 停止服務；有回合進行中時拒絕（結束碼 2），除非 --wait 或 --force
    Stop {
        #[command(flatten)]
        stop: StopArgs,
    },
    /// 重啟服務：同 stop 的閒置檢查，停止後沿用原本的位址再 start
    Restart {
        #[command(flatten)]
        stop: StopArgs,
        /// 新的服務位址（預設沿用目前的）
        #[arg(long)]
        listen: Option<String>,
        /// 重新啟動後最多等多久就緒
        #[arg(long, default_value = "30s", value_parser = service::parse_duration)]
        start_timeout: std::time::Duration,
    },
    /// 服務狀態：pid、位址、uptime、進行中的工作。執行中結束碼 0，已停 3
    Status {
        #[arg(long)]
        json: bool,
    },
    /// 開機（登入）自動啟動：macOS LaunchAgent、Linux systemd user unit
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// 健康檢查（唯讀；服務停著也能跑）。有失敗項目時結束碼 1
    Doctor {
        /// 服務位址（檢查埠）；預設是執行中服務記錄的位址（var/aaf.run.json），沒有就是 127.0.0.1:8111
        #[arg(long)]
        listen: Option<String>,
        /// 也實際啟動每個角色的 agent 做連線檢查（不送提示、不花 token）
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        json: bool,
    },
    /// 列出已註冊的功能模組
    Features,
    /// 列出所有功能宣告的權限能力
    Capabilities,
    /// 立即匯出群聊逐字稿（不必等排程；服務執行中也可以）
    Transcript {
        /// 只匯出這些群（可重複）
        #[arg(long = "room")]
        rooms: Vec<i64>,
        /// 只重寫這天（YYYY-MM-DD）以後的檔
        #[arg(long)]
        since: Option<String>,
    },
    /// 任務板（用 AAF_TOKEN／AAF_URL 呼叫 API；agent 在回合中使用）
    Task {
        #[command(subcommand)]
        cmd: client::TaskCmd,
    },
    /// 群聊（用 AAF_TOKEN／AAF_URL 呼叫 API）
    Chat {
        #[command(subcommand)]
        cmd: client::ChatCmd,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// 寫入 LaunchAgent／systemd user unit 並啟動；之後 start／stop／restart 都轉給它
    Install {
        /// 服務位址；預設沿用上次（var/aaf.run.json），沒有就是 127.0.0.1:8111
        #[arg(long)]
        listen: Option<String>,
        /// 服務名稱（預設是 home 目錄名）：io.aaf.<name>／aaf-<name>.service
        #[arg(long)]
        name: Option<String>,
        /// 取代不是 aaf 產生的同名檔（會先備份）
        #[arg(long)]
        force: bool,
        /// 只安裝，不馬上啟動（下次登入才啟動）
        #[arg(long)]
        no_start: bool,
        /// macOS：執行檔或 home 在 ~/Desktop、~/Documents、~/Downloads 也照裝（要先給 aaf 完整磁碟取用權限）
        #[arg(long)]
        allow_protected: bool,
        /// 啟動後最多等多久就緒
        #[arg(long, default_value = "30s", value_parser = service::parse_duration)]
        timeout: std::time::Duration,
    },
    /// 停止並移除（同 stop 的閒置檢查）；只移除 aaf 產生的檔
    Uninstall {
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        stop: StopArgs,
    },
    /// 安裝狀態：檔案、是否在跑、是不是這個實例。在跑結束碼 0，否則 3
    Status {
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(clap::Args)]
struct StopArgs {
    /// 不管有沒有進行中的工作直接停（進行中的回合會中斷，有未提交 attempt 的 lane 之後要 owner 解除）
    #[arg(long)]
    force: bool,
    /// 等到閒置（每 5 秒檢查、連續兩次）再停；可給上限，例如 --wait=10m（預設 30m）
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "30m", value_parser = service::parse_duration)]
    wait: Option<std::time::Duration>,
    /// 送出停止後最多等多久
    #[arg(long, default_value = "30s", value_parser = service::parse_duration)]
    timeout: std::time::Duration,
    /// 逾時後送 SIGKILL（只限不是服務管理器啟動的）
    #[arg(long)]
    kill: bool,
}

impl StopArgs {
    fn opts(&self) -> service::StopOpts {
        service::StopOpts {
            force: self.force,
            wait: self.wait,
            timeout: self.timeout,
            kill: self.kill,
        }
    }
}

/// 讀 instance 的 deploy/roles.json（沒有就是空名單）。
fn roster(home: &std::path::Path) -> anyhow::Result<aaf_runtime::dispatcher::Roster> {
    // 記住 roles.json 路徑：之後修改或建立都會被熱重載
    let mut r = aaf_runtime::dispatcher::Roster::load(home)?;
    r.agents = Some(aaf_runtime::process::Registry::new(&home.join("var"))?);
    Ok(r)
}

/// 資料庫（含 WAL）與設定檔只給本人讀寫：裡面有密碼雜湊、session、訊息。
fn private_files(home: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    for f in [
        "aaf.sqlite3",
        "aaf.sqlite3-wal",
        "aaf.sqlite3-shm",
        ".aaf.env",
    ] {
        let p = home.join(f);
        if let Ok(m) = std::fs::metadata(&p) {
            if m.permissions().mode() & 0o077 != 0 {
                if let Err(e) = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))
                {
                    tracing::warn!(file = %p.display(), error = %e, "cannot make file private");
                }
            }
        }
    }
}

/// 同一個 instance 只能有一個 aaf：在 var/aaf.lock 取得獨佔鎖，程序結束時由 OS 自動釋放。
/// `aaf status`／`doctor`／`start` 會短暫試鎖來判斷是否執行中，所以拿不到時重試約 1 秒才放棄。
fn lock_instance(home: &std::path::Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(home.join("var"))?;
    let path = home.join("var").join("aaf.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    let mut tries = 0;
    loop {
        match f.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if tries < 10 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                anyhow::bail!(
                    "another aaf is already running for {} (pid {})",
                    home.display(),
                    holder.trim()
                )
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    // 先蓋寫再截斷：讀的人不會看到空檔（頂多多看到一行換行）
    use std::os::unix::fs::FileExt;
    let pid = format!("{}\n", std::process::id());
    f.write_all_at(pid.as_bytes(), 0)?;
    f.set_len(pid.len() as u64)?;
    Ok(f)
}

/// 等關機訊號：SIGINT（Ctrl-C）或 SIGTERM（kill、aaf stop、launchd／systemd 停止）。
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGTERM; only Ctrl-C stops gracefully");
            let _ = tokio::signal::ctrl_c().await;
            return "SIGINT";
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = term.recv() => "SIGTERM",
    }
}

/// 關機時 HTTP 連線最多等多久（開著的網頁有 SSE，不設上限會一直等）。
const HTTP_DRAIN: std::time::Duration = std::time::Duration::from_secs(5);
/// 關機時停 agent 的總上限（含等回合收尾）；加上 HTTP_DRAIN 要小於 launchd／systemd 的停止逾時（30 秒）。
const AGENT_STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// 關機：停止派工並停掉所有 agent（整個程序群組），session 保留供重啟後續接。
/// 有總上限；這段期間再收到一次 SIGINT／SIGTERM 就立刻結束（剩下的 agent 由下次啟動清掉）。
async fn stop_agents(chat: &aaf_feature_chat::Shared) {
    tokio::select! {
        r = tokio::time::timeout(AGENT_STOP_LIMIT, chat.shutdown()) => match r {
            Ok(failed) => {
                for (lane, e) in failed {
                    tracing::warn!(%lane, error = %e, "agent could not be stopped; next start will reap it");
                }
            }
            Err(_) => tracing::warn!(
                "agents still stopping after {}s; next start will reap them",
                AGENT_STOP_LIMIT.as_secs()
            ),
        },
        sig = shutdown_signal() => {
            tracing::warn!(signal = sig, "second signal; exiting now, next start will reap agents");
            std::process::exit(1);
        }
    }
}

/// 所有功能模組：核心註冊（Feature）與 HTTP 註冊（HttpFeature）共用同一個實例。
fn features(
    home: &std::path::Path,
) -> anyhow::Result<(aaf_core::Registry, Vec<Arc<dyn aaf_api::HttpFeature>>)> {
    features_at(home, "127.0.0.1:8111").map(|(r, h, _)| (r, h))
}

#[allow(clippy::type_complexity)]
fn features_at(
    home: &std::path::Path,
    listen: &str,
) -> anyhow::Result<(
    aaf_core::Registry,
    Vec<Arc<dyn aaf_api::HttpFeature>>,
    Arc<aaf_feature_chat::Shared>,
)> {
    let chat = Arc::new(aaf_feature_chat::Chat::new(roster(home)?));
    chat.set_api_url(&format!("http://{listen}"));
    // 外部規則：AAF_HOOKS 或 <instance>/hooks（目錄不存在＝沒有規則）
    let hooks_dir = std::env::var_os("AAF_HOOKS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join("hooks"));
    chat.set_hooks(aaf_runtime::hooks::Hooks::new(hooks_dir, home));
    let mut r = aaf_core::Registry::default();
    r.register(Box::new(aaf_api::AuthFeature))?;
    r.register(Box::new(SharedFeature(chat.clone())))?;
    r.register(Box::new(aaf_feature_context::Context))?;
    chat.set_continuity(aaf_feature_context::runtime);
    let context = Arc::new(aaf_feature_context::http::ContextHttp::new(Arc::new(
        |core, u, lane| {
            core.db.with(|c| {
                let Some(room) = lane.room else {
                    return Ok(false);
                };
                let r = aaf_feature_chat::store::room(c, room)?;
                Ok(r.is_some_and(|r| r.state != "deleted")
                    && (u.rank == aaf_core::perms::Rank::Owner
                        || aaf_feature_chat::store::is_member(c, room, &u.name)?))
            })
        },
    )));
    let files = Arc::new(aaf_feature_files::Files::new(home));
    r.register(Box::new(SharedFeature(files.clone())))?;
    let skills = Arc::new(aaf_feature_skills::Skills::new(home));
    r.register(Box::new(SharedFeature(skills.clone())))?;
    // 指派的 skill：每個 lane 第一回合與內容變更時送給 agent
    let sk = skills.clone();
    // 群級 lead：被指定的角色只在該群自動帶 `lead` 包
    let ch = chat.shared();
    chat.set_context(move |lane| {
        let (extra, drop): (&[&str], &[&str]) = match lane.room.and_then(|r| ch.room_lead(r)) {
            Some(l) if l == lane.role => (&["lead"], &[]),
            // 群已指定別人當 lead：這個角色在本群不帶 lead 包（即使全域是 lead）
            Some(_) => (&[], &["lead"]),
            None => (&[], &[]),
        };
        sk.context_for_with(&lane.role, lane.room, extra, drop)
    });
    let agents = Arc::new(aaf_feature_agents::Agents::new(home, chat.shared()));
    r.register(Box::new(SharedFeature(agents.clone())))?;
    let shared = chat.shared();
    Ok((r, vec![chat, files, skills, agents, context], shared))
}

/// 讓 Arc<T: Feature> 能放進 Registry（Registry 收 Box<dyn Feature>）。
struct SharedFeature<T>(Arc<T>);

impl<T: aaf_core::Feature> aaf_core::Feature for SharedFeature<T> {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn migrations(&self) -> &'static [aaf_core::storage::Migration] {
        self.0.migrations()
    }
    fn capabilities(&self) -> &'static [aaf_core::perms::CapabilityDef] {
        self.0.capabilities()
    }
    fn config(&self) -> &'static [aaf_core::config::ConfigKey] {
        self.0.config()
    }
    fn subscribers(&self) -> Vec<Arc<dyn aaf_core::feature::Subscriber>> {
        self.0.subscribers()
    }
    fn jobs(&self) -> Vec<aaf_core::jobs::Job> {
        self.0.jobs()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        // 背景執行（aaf start、launchd、systemd）時 stdout 導到檔案：不寫 ANSI 色碼
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Transcript { rooms, since } => {
            let p = cli.home.join("aaf.sqlite3");
            if !p.exists() {
                anyhow::bail!("no AA Forum database at {}", p.display());
            }
            let db = aaf_core::storage::Db::open(&p)?;
            let opts = aaf_core::feature::BootOptions {
                env_file: Some(cli.home.join(".aaf.env")),
                ..Default::default()
            };
            let chat = Arc::new(aaf_feature_chat::Chat::new(roster(&cli.home)?));
            let mut reg = aaf_core::Registry::default();
            reg.register(Box::new(aaf_api::AuthFeature))?;
            reg.register(Box::new(SharedFeature(chat.clone())))?;
            let core = aaf_core::Core::boot(&reg, db, opts)?;
            let rooms = (!rooms.is_empty()).then_some(rooms);
            let rep =
                chat.shared()
                    .export_transcripts(&core, rooms.as_deref(), since.as_deref())?;
            println!(
                "exported {} rooms, {} files updated → {}",
                rep.rooms,
                rep.written.len(),
                chat.shared().transcript_root(&core).display()
            );
        }
        Cmd::Doctor {
            listen,
            probe,
            json,
        } => {
            let listen = listen
                .or_else(|| service::read_run_info(&cli.home).map(|i| i.listen))
                .unwrap_or_else(|| service::DEFAULT_LISTEN.into());
            let checks = doctor::run(&cli.home, &doctor::Opts { listen, probe }).await;
            if !doctor::print(&checks, json) {
                std::process::exit(1);
            }
        }
        Cmd::Task { cmd } => client::task(cmd)?,
        Cmd::Chat { cmd } => client::chat(cmd)?,
        Cmd::Context { cmd } => client::context(cmd)?,
        Cmd::Features => {
            for n in features(&cli.home)?.0.names() {
                println!("{n}");
            }
        }
        Cmd::Capabilities => {
            let core = aaf_core::Core::prepare(
                &features(&cli.home)?.0,
                aaf_core::storage::Db::open_in_memory()?,
                Default::default(),
            )?;
            for (id, d) in core.policy.list() {
                println!("{id}\t{d}");
            }
        }
        Cmd::Start { listen, timeout } => {
            std::process::exit(service::start(&cli.home, listen, timeout).await?)
        }
        Cmd::Stop { stop } => std::process::exit(service::stop(&cli.home, &stop.opts()).await?),
        Cmd::Restart {
            stop,
            listen,
            start_timeout,
        } => std::process::exit(
            service::restart(&cli.home, &stop.opts(), listen, start_timeout).await?,
        ),
        Cmd::Status { json } => std::process::exit(service::status(&cli.home, json)?),
        Cmd::Service { cmd } => std::process::exit(match cmd {
            ServiceCmd::Install {
                listen,
                name,
                force,
                no_start,
                timeout,
                allow_protected,
            } => {
                let o = install::InstallOpts {
                    listen,
                    name,
                    force,
                    no_start,
                    timeout,
                    allow_protected,
                };
                install::install(&cli.home, &o).await?
            }
            ServiceCmd::Uninstall { name, stop } => {
                install::uninstall(&cli.home, name.as_deref(), &stop.opts()).await?
            }
            ServiceCmd::Status { name } => install::status(&cli.home, name.as_deref())?,
        }),
        Cmd::Up { listen, detach } => {
            if detach {
                nix::unistd::setsid()?;
            }
            if !listen.starts_with("127.0.0.1:")
                && !listen.starts_with("localhost:")
                && !listen.starts_with("[::1]:")
            {
                tracing::warn!("listening on non-loopback address {listen}; use a reverse proxy with TLS for remote access");
            }
            std::fs::create_dir_all(&cli.home)?;
            let _lock = lock_instance(&cli.home)?;
            // 上次異常結束留下的 agent：身分（pid＋出生時間）相符才終止
            let agents = aaf_runtime::process::Registry::new(&cli.home.join("var"))?;
            for (rec, outcome) in
                aaf_runtime::process::reap_orphans(&agents, std::time::Duration::from_secs(3)).await
            {
                tracing::warn!(pid = rec.pid, command = %rec.command, ?outcome, "orphan agent from previous run");
            }
            let db = aaf_core::storage::Db::open(&cli.home.join("aaf.sqlite3"))?;
            private_files(&cli.home);
            // 已持有鎖、核心還沒開機（還沒派工）：上次留下的 running 回合不可能還在跑
            match aaf_feature_agents::turns::close_stale(&db) {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    turns = n,
                    "marked unfinished turns from previous run as incomplete"
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "cannot close unfinished turns from previous run")
                }
            }
            let opts = aaf_core::feature::BootOptions {
                env_file: Some(cli.home.join(".aaf.env")),
                ..Default::default()
            };
            let (reg, http, chat) = features_at(&cli.home, &listen)?;
            // 埠與 run.json 在核心開機（開始派工）之前處理：失敗時還沒有 agent 要收
            let l = tokio::net::TcpListener::bind(&listen).await?;
            service::write_run_info(&cli.home, &listen)?;
            let core = aaf_core::Core::boot(&reg, db, opts)?;
            if let Err(e) = aaf_feature_context::config::Settings::parse(&core.config) {
                stop_agents(&chat).await;
                return Err(e.into());
            }
            let app = aaf_api::app(core, &http);
            tracing::info!(
                "AA Forum on http://{listen}/app/  (home: {})",
                cli.home.display()
            );
            let (stop_http, http_stopped) = tokio::sync::oneshot::channel::<()>();
            let mut server = tokio::spawn(async move {
                axum::serve(l, app)
                    .with_graceful_shutdown(async {
                        let _ = http_stopped.await;
                    })
                    .await
            });
            // HTTP 自己結束（出錯）也要走同樣的收尾：先停 agent，不留孤兒
            let early = tokio::select! {
                sig = shutdown_signal() => {
                    tracing::info!(signal = sig, "shutting down");
                    None
                }
                r = &mut server => Some(r),
            };
            // 1. 停止派工並停掉所有 agent
            stop_agents(&chat).await;
            // 2. HTTP：不再收新連線，等進行中的請求最多 HTTP_DRAIN，之後直接關
            let served = match early {
                Some(r) => r,
                None => {
                    let _ = stop_http.send(());
                    match tokio::time::timeout(HTTP_DRAIN, &mut server).await {
                        Ok(r) => r,
                        Err(_) => {
                            tracing::info!(
                                "open HTTP connections closed after {}s",
                                HTTP_DRAIN.as_secs()
                            );
                            server.abort();
                            Ok(Ok(()))
                        }
                    }
                }
            };
            served??;
            tracing::info!("stopped");
        }
    }
    Ok(())
}
