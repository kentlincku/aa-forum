//! 回合紀錄（Dashboard 用）：由 runtime.turn.* 事件寫入 agents_turns。

use aaf_contract::EventEnvelope;
use aaf_core::storage::Migration;
use aaf_core::Core;
use rusqlite::params;
use serde_json::{json, Value};
use std::sync::Arc;

pub static MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "turns",
    sql: "
        CREATE TABLE agents_turns(
            id INTEGER PRIMARY KEY,
            role TEXT NOT NULL,
            room INTEGER,
            started_at INTEGER NOT NULL,
            ended_at INTEGER,
            -- running／ok／failed／incomplete／start_failed
            outcome TEXT NOT NULL,
            error TEXT,
            messages INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX agents_turns_started ON agents_turns(started_at);
        CREATE INDEX agents_turns_role ON agents_turns(role, started_at);
    ",
}];

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Recorder;

#[async_trait::async_trait]
impl aaf_core::feature::Subscriber for Recorder {
    fn name(&self) -> &'static str {
        "agents_turns"
    }
    fn kinds(&self) -> &'static [&'static str] {
        &["runtime.turn.", "runtime.lane.start_failed"]
    }
    async fn handle(&self, core: &Arc<Core>, ev: &EventEnvelope) -> aaf_core::Result<()> {
        let lane = &ev.payload["lane"];
        let role = lane["role"].as_str().unwrap_or_default().to_string();
        let room = lane["room"].as_i64();
        let t = now();
        core.db.with(|c| {
            match ev.kind.as_str() {
                "runtime.turn.started" => {
                    let n = ev.payload["messages"].as_array().map(|a| a.len()).unwrap_or(0) as i64;
                    c.execute("INSERT INTO agents_turns(role, room, started_at, outcome, messages) VALUES (?1, ?2, ?3, 'running', ?4)", params![role, room, t, n])?;
                }
                "runtime.lane.start_failed" => {
                    let err = ev.payload["error"].as_str().map(|s| s.chars().take(500).collect::<String>());
                    c.execute("INSERT INTO agents_turns(role, room, started_at, ended_at, outcome, error) VALUES (?1, ?2, ?3, ?3, 'start_failed', ?4)", params![role, room, t, err])?;
                }
                k => {
                    let outcome = match k {
                        "runtime.turn.ended" => "ok",
                        "runtime.turn.failed" => "failed",
                        "runtime.turn.incomplete" => "incomplete",
                        _ => return Ok(()),
                    };
                    let err = ev.payload["error"].as_str().or(ev.payload["stop_reason"].as_str()).map(|s| s.chars().take(500).collect::<String>());
                    c.execute(
                        "UPDATE agents_turns SET ended_at = ?1, outcome = ?2, error = ?3
                         WHERE id = (SELECT id FROM agents_turns WHERE role = ?4 AND room IS ?5 AND outcome = 'running' ORDER BY id DESC LIMIT 1)",
                        params![t, outcome, err, role, room],
                    )?;
                }
            }
            Ok(())
        })
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(
        json!({"id": r.get::<_, i64>(0)?, "role": r.get::<_, String>(1)?, "room": r.get::<_, Option<i64>>(2)?,
        "started_at": r.get::<_, i64>(3)?, "ended_at": r.get::<_, Option<i64>>(4)?, "outcome": r.get::<_, String>(5)?,
        "error": r.get::<_, Option<String>>(6)?, "messages": r.get::<_, i64>(7)?}),
    )
}

const COLS: &str = "id, role, room, started_at, ended_at, outcome, error, messages";

fn room_filter(rooms: &[i64]) -> String {
    // 只含數字，直接內嵌安全；沒有可見的群時用不可能的條件
    if rooms.is_empty() {
        "0".into()
    } else {
        format!(
            "(room IS NULL OR room IN ({}))",
            rooms
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

/// 服務啟動時（已持有 instance 鎖、核心還沒開機派工）：上次沒結束的回合不可能還在跑
/// （被 kill、crash、關機時事件沒寫進來），改成 incomplete。結束時間不知道，維持 NULL。
/// 新資料庫還沒有這張表時什麼都不做。
pub fn close_stale(db: &aaf_core::storage::Db) -> aaf_core::Result<usize> {
    db.with(|c| {
        let exists: i64 = c.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'agents_turns'",
            [],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Ok(0);
        }
        Ok(c.execute(
            "UPDATE agents_turns SET outcome = 'incomplete', error = 'service restarted' WHERE outcome = 'running'",
            [],
        )?)
    })
}

pub fn recent(core: &Core, rooms: &[i64], limit: i64) -> aaf_core::Result<Vec<Value>> {
    core.db.with(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM agents_turns WHERE {} ORDER BY id DESC LIMIT ?1",
            room_filter(rooms)
        ))?;
        let v = st
            .query_map([limit], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}

pub fn last_for(core: &Core, role: &str) -> aaf_core::Result<Value> {
    core.db.with(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM agents_turns WHERE role = ?1 ORDER BY id DESC LIMIT 1"
        ))?;
        let v = st.query_map([role], row)?.next().transpose()?;
        Ok(v.unwrap_or(Value::Null))
    })
}

pub fn stats(core: &Core, rooms: &[i64], secs: i64) -> aaf_core::Result<Value> {
    core.db.with(|c| {
        let mut st = c.prepare(&format!("SELECT outcome, COUNT(*), AVG(ended_at - started_at) FROM agents_turns WHERE started_at >= ?1 AND {} GROUP BY outcome", room_filter(rooms)))?;
        let mut m = serde_json::Map::new();
        for r in st.query_map([now() - secs], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<f64>>(2)?)))? {
            let (o, n, avg) = r?;
            m.insert(o, json!({"count": n, "avg_secs": avg.map(|a| (a * 10.0).round() / 10.0)}));
        }
        Ok(Value::Object(m))
    })
}

/// 警示：最近失敗／啟動失敗（每個角色＋群只留最新一筆，且之後沒有成功過）。
pub fn alerts(core: &Core, rooms: &[i64], secs: i64) -> aaf_core::Result<Vec<Value>> {
    core.db.with(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM agents_turns t WHERE started_at >= ?1 AND outcome IN ('failed', 'start_failed') AND {}
               AND NOT EXISTS (SELECT 1 FROM agents_turns o WHERE o.role = t.role AND o.room IS t.room AND o.id > t.id AND o.outcome = 'ok')
               AND t.id = (SELECT MAX(id) FROM agents_turns x WHERE x.role = t.role AND x.room IS t.room AND x.outcome IN ('failed', 'start_failed'))
             ORDER BY id DESC LIMIT 20",
            room_filter(rooms)
        ))?;
        let v = st.query_map([now() - secs], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}
