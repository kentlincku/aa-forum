//! 群自動核准（源自先前的 Python 版）。
//!
//! 開啟後，該群 agent 的權限請求自動選「允許一次」（沒有就「一律允許」），並發事件留紀錄；
//! 拒絕類選項永遠不會被自動選。只有人能開關，開關時在群裡發系統公告。

use crate::store::{self, now, write};
use aaf_core::{Core, Error, Result};
use aaf_runtime::dispatcher::Roster;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Approval {
    pub room: i64,
    pub on: bool,
    pub until: Option<String>,
    pub by: Option<String>,
    pub at: Option<i64>,
    pub expired: bool,
}

pub fn today(tz: Option<&str>) -> String {
    let z = match tz.filter(|t| !t.trim().is_empty()) {
        Some(t) => jiff::tz::TimeZone::get(t).unwrap_or_else(|_| jiff::tz::TimeZone::system()),
        None => jiff::tz::TimeZone::system(),
    };
    jiff::Timestamp::now()
        .to_zoned(z)
        .strftime("%Y-%m-%d")
        .to_string()
}

pub fn state(c: &rusqlite::Connection, room: i64, today: &str) -> Result<Approval> {
    let r: Option<(bool, Option<String>, String, i64)> = c
        .query_row(
            "SELECT enabled, until, actor, changed_at FROM chat_room_approval WHERE room = ?1",
            [room],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(match r {
        None => Approval {
            room,
            on: false,
            until: None,
            by: None,
            at: None,
            expired: false,
        },
        Some((en, until, by, at)) => {
            let expired = en && until.as_deref().is_some_and(|u| u < today);
            Approval {
                room,
                on: en && !expired,
                until,
                by: Some(by),
                at: Some(at),
                expired,
            }
        }
    })
}

pub fn valid_date(d: &str) -> bool {
    jiff::civil::Date::strptime("%Y-%m-%d", d).is_ok()
}

#[allow(clippy::too_many_arguments)]
pub fn set(
    core: &Core,
    roster: &Roster,
    room: i64,
    actor: &str,
    on: bool,
    until: Option<&str>,
    reason: &str,
    today: &str,
) -> Result<(Approval, bool)> {
    if let Some(u) = until {
        if !valid_date(u) {
            return Err(Error::Other("until must be YYYY-MM-DD".into()));
        }
        if on && u < today {
            return Err(Error::Other("until cannot be in the past".into()));
        }
    }
    write(core, |tx, evs| {
        let r = store::room(tx, room)?.ok_or_else(|| Error::Other("room not found".into()))?;
        if r.state != "active" {
            return Err(Error::Other(format!("room is {}", r.state)));
        }
        let old = state(tx, room, today)?;
        let until = if on { until.map(str::to_string) } else { None };
        if old.on == on && (!on || old.until == until) {
            return Ok((old, false));
        }
        tx.execute(
            "INSERT INTO chat_room_approval(room, enabled, until, actor, changed_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(room) DO UPDATE SET enabled = excluded.enabled, until = excluded.until, actor = excluded.actor, changed_at = excluded.changed_at",
            params![room, on, until, actor, now()],
        )?;
        let mut body = if on {
            let span = until
                .as_deref()
                .map(|u| format!("until {u} (inclusive)"))
                .unwrap_or_else(|| "with no end date, until turned off".into());
            format!("Auto-approve ON by {actor} ({span}). Agent permission requests in this room are allowed once automatically; reject options are never chosen.")
        } else {
            format!("Auto-approve OFF by {actor}. Permission requests go back to manual approval.")
        };
        if !reason.trim().is_empty() {
            body.push_str(&format!("\nReason: {}", reason.trim()));
        }
        store::post_system(tx, evs, roster, room, &body)?;
        let st = state(tx, room, today)?;
        evs.push(aaf_core::events::EventBus::record(
            tx,
            "chat.approval.changed",
            1,
            serde_json::json!({"room": room, "approval": st, "by": actor}),
        )?);
        Ok((st, true))
    })
}
