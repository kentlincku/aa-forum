//! 儲存與遷移：單一 SQLite（WAL），每個 feature 以自己的命名空間套用有序 migration。

use crate::{Error, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// 一筆 migration。同一命名空間內 version 必須嚴格遞增；已套用的不可改名。
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

const CORE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS _migrations (
    namespace TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    applied_at INTEGER NOT NULL,
    PRIMARY KEY (namespace, version)
);
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    version INTEGER NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
";

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(CORE_SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 在連線上執行一段同步工作（短交易用；長工作請自行 spawn_blocking）。
    pub fn with<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        let mut c = self
            .conn
            .lock()
            .map_err(|_| Error::Other("db mutex poisoned".into()))?;
        f(&mut c)
    }

    /// 套用某命名空間的 migration；回傳本次新套用的數量。每筆 migration 在自己的交易內。
    pub fn migrate(&self, namespace: &str, migrations: &[Migration]) -> Result<usize> {
        for w in migrations.windows(2) {
            if w[1].version <= w[0].version {
                return Err(Error::Migration(format!(
                    "{namespace}: versions must strictly increase ({} then {})",
                    w[0].version, w[1].version
                )));
            }
        }
        self.with(|c| {
            let mut applied = 0;
            for m in migrations {
                let existing: Option<String> = c
                    .query_row("SELECT name FROM _migrations WHERE namespace=?1 AND version=?2", (namespace, m.version), |r| r.get(0))
                    .map(Some)
                    .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
                match existing {
                    Some(name) if name == m.name => continue,
                    Some(name) => {
                        return Err(Error::Migration(format!("{namespace} v{}: applied as '{name}', now named '{}'", m.version, m.name)));
                    }
                    None => {}
                }
                let tx = c.transaction()?;
                tx.execute_batch(m.sql)
                    .map_err(|e| Error::Migration(format!("{namespace} v{} {}: {e}", m.version, m.name)))?;
                tx.execute(
                    "INSERT INTO _migrations(namespace, version, name, applied_at) VALUES (?1, ?2, ?3, ?4)",
                    (namespace, m.version, m.name, now()),
                )?;
                tx.commit()?;
                applied += 1;
            }
            Ok(applied)
        })
    }
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &[Migration] = &[
        Migration {
            version: 1,
            name: "create t",
            sql: "CREATE TABLE t(id INTEGER PRIMARY KEY);",
        },
        Migration {
            version: 2,
            name: "add col",
            sql: "ALTER TABLE t ADD COLUMN v TEXT;",
        },
    ];

    #[test]
    fn applies_once_and_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.migrate("demo", M).unwrap(), 2);
        assert_eq!(db.migrate("demo", M).unwrap(), 0);
        db.with(|c| Ok(c.execute("INSERT INTO t(v) VALUES ('x')", [])?))
            .unwrap();
    }

    #[test]
    fn rejects_renamed_and_unordered() {
        let db = Db::open_in_memory().unwrap();
        db.migrate("demo", &M[..1]).unwrap();
        let renamed = [Migration {
            name: "other",
            ..M[0]
        }];
        assert!(matches!(
            db.migrate("demo", &renamed),
            Err(Error::Migration(_))
        ));
        let unordered = [M[1], M[0]];
        assert!(db.migrate("x", &unordered).is_err());
    }

    #[test]
    fn failed_migration_rolls_back() {
        let db = Db::open_in_memory().unwrap();
        let bad = [Migration {
            version: 1,
            name: "bad",
            sql: "CREATE TABLE a(id INT); NOT SQL;",
        }];
        assert!(db.migrate("bad", &bad).is_err());
        let n: i64 = db
            .with(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='a'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn namespaces_are_independent() {
        let db = Db::open_in_memory().unwrap();
        db.migrate(
            "a",
            &[Migration {
                version: 1,
                name: "a1",
                sql: "CREATE TABLE a(x);",
            }],
        )
        .unwrap();
        assert_eq!(
            db.migrate(
                "b",
                &[Migration {
                    version: 1,
                    name: "b1",
                    sql: "CREATE TABLE b(x);"
                }]
            )
            .unwrap(),
            1
        );
    }
}
