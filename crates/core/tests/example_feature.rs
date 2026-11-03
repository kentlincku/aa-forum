//! 擴充點最小範例：一個 feature 同時使用 migration、能力、設定、事件訂閱者、背景工作。
//! 新功能可照抄此檔的結構。

use aaf_contract::EventEnvelope;
use aaf_core::config::ConfigKey;
use aaf_core::events::EventBus;
use aaf_core::feature::{BootOptions, Subscriber};
use aaf_core::jobs::Job;
use aaf_core::perms::{CapabilityDef, Rank};
use aaf_core::storage::{Db, Migration};
use aaf_core::{Core, Feature, Registry};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

struct Notes;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "notes",
        sql: "CREATE TABLE notes_items(id INTEGER PRIMARY KEY, body TEXT NOT NULL);",
    },
    Migration {
        version: 2,
        name: "counter",
        sql:
            "CREATE TABLE notes_counter(n INTEGER NOT NULL); INSERT INTO notes_counter VALUES (0);",
    },
];
const CAPS: &[CapabilityDef] = &[
    CapabilityDef {
        id: "notes.item.create",
        description: "新增筆記",
        default_ranks: &[Rank::Human, Rank::Lead],
    },
    CapabilityDef {
        id: "notes.item.purge",
        description: "清空筆記",
        default_ranks: &[],
    },
];
const CONFIG: &[ConfigKey] = &[ConfigKey {
    key: "AAF_NOTES_PREFIX",
    default: "note:",
    description: "筆記前綴",
}];

/// 業務操作範例：資料與事件同一交易，commit 後廣播。
fn create(core: &Core, rank: Rank, body: &str) -> aaf_core::Result<EventEnvelope> {
    core.policy.check(rank, "notes.item.create")?;
    let body = format!("{}{body}", core.config.get("AAF_NOTES_PREFIX")?);
    let ev = core.db.with(|c| {
        let tx = c.transaction()?;
        tx.execute("INSERT INTO notes_items(body) VALUES (?1)", [&body])?;
        let id = tx.last_insert_rowid();
        let ev = EventBus::record(
            &tx,
            "notes.item.created",
            1,
            json!({"id": id, "body": body}),
        )?;
        tx.commit()?;
        Ok(ev)
    })?;
    core.bus.announce(ev.clone());
    Ok(ev)
}

struct CountCreated;

#[async_trait]
impl Subscriber for CountCreated {
    fn name(&self) -> &'static str {
        "count_created"
    }
    fn kinds(&self) -> &'static [&'static str] {
        &["notes.item."]
    }
    async fn handle(&self, core: &Arc<Core>, _ev: &EventEnvelope) -> aaf_core::Result<()> {
        core.db
            .with(|c| Ok(c.execute("UPDATE notes_counter SET n = n + 1", [])?))?;
        Ok(())
    }
}

impl Feature for Notes {
    fn name(&self) -> &'static str {
        "notes"
    }
    fn migrations(&self) -> &'static [Migration] {
        MIGRATIONS
    }
    fn capabilities(&self) -> &'static [CapabilityDef] {
        CAPS
    }
    fn config(&self) -> &'static [ConfigKey] {
        CONFIG
    }
    fn subscribers(&self) -> Vec<Arc<dyn Subscriber>> {
        vec![Arc::new(CountCreated)]
    }
    fn jobs(&self) -> Vec<Job> {
        vec![Job::every(
            "heartbeat",
            Duration::from_millis(20),
            |core| async move {
                core.bus.publish("notes.job.tick", 1, json!({}))?;
                Ok(())
            },
        )]
    }
}

async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met in time");
}

#[tokio::test]
async fn feature_uses_every_extension_point() {
    let mut reg = Registry::default();
    reg.register(Box::new(Notes)).unwrap();
    let opts = BootOptions {
        env: vec![("AAF_NOTES_PREFIX".into(), "n/".into())],
        ..Default::default()
    };
    let core = Core::boot(&reg, Db::open_in_memory().unwrap(), opts).unwrap();

    // 權限：Worker 無權、Owner 可以
    assert!(create(&core, Rank::Worker, "x").is_err());
    let ev = create(&core, Rank::Owner, "hello").unwrap();
    assert_eq!(ev.payload["body"], "n/hello"); // 設定由環境變數覆寫

    // 訂閱者：notes.item.* 與 notes.job.tick 都符合前綴 "notes.item." 嗎？只有前者
    let count = || {
        core.db
            .with(|c| Ok(c.query_row("SELECT n FROM notes_counter", [], |r| r.get::<_, i64>(0))?))
            .unwrap()
    };
    wait_until(|| count() == 1).await;

    // 背景工作：持續發 tick 事件
    wait_until(|| {
        core.bus
            .since(0, 100)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "notes.job.tick")
            .count()
            >= 2
    })
    .await;
    assert_eq!(count(), 1, "tick 事件不應被 notes.item. 訂閱者處理");

    let states = core.service_states();
    assert_eq!(states.len(), 2);
}

#[test]
fn capability_must_be_namespaced_by_feature() {
    struct Bad;
    impl Feature for Bad {
        fn name(&self) -> &'static str {
            "bad"
        }
        fn capabilities(&self) -> &'static [CapabilityDef] {
            &[CapabilityDef {
                id: "other.x.y",
                description: "",
                default_ranks: &[],
            }]
        }
    }
    let mut reg = Registry::default();
    reg.register(Box::new(Bad)).unwrap();
    assert!(Core::prepare(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![],
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn registry_rejects_bad_names_and_duplicates() {
    let mut reg = Registry::default();
    reg.register(Box::new(Notes)).unwrap();
    assert!(reg.register(Box::new(Notes)).is_err());
    struct Core_;
    impl Feature for Core_ {
        fn name(&self) -> &'static str {
            "core"
        }
    }
    assert!(reg.register(Box::new(Core_)).is_err());
}

#[test]
fn migrations_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.sqlite3");
    let mut reg = Registry::default();
    reg.register(Box::new(Notes)).unwrap();
    let o = || BootOptions {
        env: vec![],
        ..Default::default()
    };
    Core::prepare(&reg, Db::open(&path).unwrap(), o()).unwrap();
    let core = Core::prepare(&reg, Db::open(&path).unwrap(), o()).unwrap();
    let n: i64 = core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM _migrations WHERE namespace='notes'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(n, 2);
}
