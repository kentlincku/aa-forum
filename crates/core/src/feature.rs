//! 功能模組與核心啟動：註冊 → 套 migration → 宣告能力與設定 → 啟動訂閱者與背景工作。

use crate::config::{Config, ConfigKey};
use crate::events::EventBus;
use crate::jobs::{self, Job};
use crate::perms::{CapabilityDef, Policy};
use crate::storage::{Db, Migration};
use crate::supervisor::{self, RestartPolicy, ServiceState};
use crate::{Error, Result};
use aaf_contract::EventEnvelope;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::watch;

/// 事件訂閱者：只收 `kinds` 前綴相符的事件。
#[async_trait]
pub trait Subscriber: Send + Sync {
    fn name(&self) -> &'static str;
    /// 事件 kind 前綴，例如 `["chat.message."]`；空陣列＝全部。
    fn kinds(&self) -> &'static [&'static str];
    async fn handle(&self, core: &Arc<Core>, ev: &EventEnvelope) -> Result<()>;
}

/// 功能模組。除 `name` 外皆有預設（空），新功能只覆寫需要的擴充點。
pub trait Feature: Send + Sync {
    /// 唯一名稱：migration 命名空間、事件 kind 與能力 id 的前綴。
    fn name(&self) -> &'static str;
    fn migrations(&self) -> &'static [Migration] {
        &[]
    }
    fn capabilities(&self) -> &'static [CapabilityDef] {
        &[]
    }
    fn config(&self) -> &'static [ConfigKey] {
        &[]
    }
    fn subscribers(&self) -> Vec<Arc<dyn Subscriber>> {
        Vec::new()
    }
    fn jobs(&self) -> Vec<Job> {
        Vec::new()
    }
}

#[derive(Default)]
pub struct Registry {
    features: Vec<Box<dyn Feature>>,
}

impl Registry {
    pub fn register(&mut self, f: Box<dyn Feature>) -> Result<()> {
        let n = f.name();
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_lowercase() || c == '_') || n == "core" {
            return Err(Error::Feature(format!("invalid feature name: {n:?}")));
        }
        if self.features.iter().any(|x| x.name() == n) {
            return Err(Error::Feature(format!("feature already registered: {n}")));
        }
        self.features.push(f);
        Ok(())
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.features.iter().map(|f| f.name()).collect()
    }
}

/// 啟動後的核心：所有 feature 共用的 context。
pub struct Core {
    pub db: Db,
    pub bus: EventBus,
    pub policy: Policy,
    pub config: Config,
    pub features: Vec<&'static str>,
    services: std::sync::Mutex<HashMap<String, watch::Receiver<ServiceState>>>,
}

pub struct BootOptions {
    pub env_file: Option<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub bus_capacity: usize,
}

impl Default for BootOptions {
    fn default() -> Self {
        Self {
            env_file: None,
            env: std::env::vars().collect(),
            bus_capacity: 1024,
        }
    }
}

impl Core {
    /// 套 migration、宣告能力與設定（同步、可在測試中不啟動 runtime）。
    pub fn prepare(registry: &Registry, db: Db, opts: BootOptions) -> Result<Core> {
        let mut policy = Policy::default();
        let mut config = Config::default();
        for f in &registry.features {
            let ns = f.name();
            db.migrate(ns, f.migrations())?;
            for c in f.capabilities() {
                if !c.id.starts_with(&format!("{ns}.")) {
                    return Err(Error::Feature(format!(
                        "capability {} must start with '{ns}.'",
                        c.id
                    )));
                }
                policy.declare(c)?;
            }
            for k in f.config() {
                config.declare(k)?;
            }
        }
        config.load(opts.env_file.as_deref(), opts.env)?;
        Ok(Core {
            bus: EventBus::new(db.clone(), opts.bus_capacity),
            db,
            policy,
            config,
            features: registry.names(),
            services: Default::default(),
        })
    }

    /// 完整啟動：prepare＋在 tokio 上啟動訂閱者與背景工作（皆受 supervisor 監督）。
    pub fn boot(registry: &Registry, db: Db, opts: BootOptions) -> Result<Arc<Core>> {
        let core = Arc::new(Self::prepare(registry, db, opts)?);
        let start_seq = core
            .bus
            .since(0, usize::MAX)?
            .last()
            .map(|e| e.seq)
            .unwrap_or(0);
        for f in &registry.features {
            for s in f.subscribers() {
                let key = format!("{}:sub:{}", f.name(), s.name());
                let rx = start_subscriber(s, core.clone(), start_seq);
                core.services.lock().unwrap().insert(key, rx);
            }
            for j in f.jobs() {
                let key = format!("{}:job:{}", f.name(), j.name);
                let rx = jobs::start(j, core.clone());
                core.services.lock().unwrap().insert(key, rx);
            }
        }
        Ok(core)
    }

    /// 各背景服務目前狀態（doctor／Dashboard 用）。
    pub fn service_states(&self) -> Vec<(String, ServiceState)> {
        let mut v: Vec<_> = self
            .services
            .lock()
            .unwrap()
            .iter()
            .map(|(k, rx)| (k.clone(), rx.borrow().clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

fn start_subscriber(
    s: Arc<dyn Subscriber>,
    core: Arc<Core>,
    start_seq: u64,
) -> watch::Receiver<ServiceState> {
    // 處理進度存在記憶體：重啟後從最後成功處理的事件之後繼續，不漏也不重複。
    let progress = Arc::new(std::sync::atomic::AtomicU64::new(start_seq));
    supervisor::spawn(s.name(), RestartPolicy::default(), move || {
        let (s, core, progress) = (s.clone(), core.clone(), progress.clone());
        Box::pin(async move {
            let mut sub = core
                .bus
                .subscribe()
                .from_seq(progress.load(std::sync::atomic::Ordering::SeqCst));
            loop {
                let ev = sub.next().await?;
                if s.kinds().is_empty() || s.kinds().iter().any(|k| ev.kind.starts_with(k)) {
                    s.handle(&core, &ev).await?;
                }
                progress.store(ev.seq, std::sync::atomic::Ordering::SeqCst);
            }
        })
    })
}
