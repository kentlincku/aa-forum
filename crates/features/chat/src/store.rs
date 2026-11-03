//! 資料存取：所有寫入都在交易內，事件與資料同交易記錄、commit 後廣播。

use aaf_contract::EventEnvelope;
use aaf_core::events::EventBus;
use aaf_core::storage::Db;
use aaf_core::{Core, Error, Result};
use aaf_runtime::dispatcher::{Dispatcher, Roster};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::Serialize;
use serde_json::json;

/// 群裡每則（非系統）訊息都投遞給該群的 lead（AAF_LEAD_SEES_ALL，預設開）。
pub static LEAD_SEES_ALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// 防 agent 互相叫醒停不下來（AAF_CHAT_AGENT_CHAIN，預設 8；0＝關閉）：同群自上一則人類發言起，
/// 角色連發超過這麼多則，角色的發言就不再叫醒其他角色，直到有人發言。
pub static AGENT_CHAIN_LIMIT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(8);
/// 設定檔（.aaf.env／環境變數）給的 AGENT_CHAIN_LIMIT；頁面清掉自訂值時回到它。
pub static AGENT_CHAIN_CONFIG: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(8);
/// 頁面可改的上限（防誤設成極大值等於關不掉）。
pub const AGENT_CHAIN_MAX: usize = 100;

/// 頁面設定（chat_settings）：沒設過回 None。
pub fn setting(c: &rusqlite::Connection, key: &str) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT value FROM chat_settings WHERE key = ?1",
        [key],
        |r| r.get(0),
    )
    .optional()?)
}

/// 寫入或清除（None）頁面設定。
pub fn set_setting(
    c: &rusqlite::Connection,
    key: &str,
    value: Option<&str>,
    by: &str,
) -> Result<()> {
    match value {
        Some(v) => c.execute(
            "INSERT INTO chat_settings(key, value, by, at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, by = excluded.by, at = excluded.at",
            params![key, v, by, now()],
        )?,
        None => c.execute("DELETE FROM chat_settings WHERE key = ?1", [key])?,
    };
    Ok(())
}

/// 套用 agent 連發上限：頁面值優先，否則設定檔值。回傳 (生效值, 是否來自頁面)。
pub fn apply_agent_chain(c: &rusqlite::Connection) -> Result<(usize, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    let page = setting(c, "agent_chain")?.and_then(|v| v.parse::<usize>().ok());
    let v = page.unwrap_or_else(|| AGENT_CHAIN_CONFIG.load(Relaxed));
    AGENT_CHAIN_LIMIT.store(v, Relaxed);
    Ok((v, page.is_some()))
}

pub const MAX_BODY: usize = 100_000;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Room {
    pub id: i64,
    pub name: String,
    pub state: String,
    pub frozen: bool,
    pub folder: String,
    pub pinned: bool,
    pub created_by: String,
    pub created_at: i64,
    pub deleted_at: Option<i64>,
    pub deleted_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Message {
    pub id: i64,
    pub room: i64,
    pub author: String,
    pub author_kind: String,
    pub body: String,
    pub reply_to: Option<i64>,
    pub thread_id: Option<i64>,
    pub created_at: i64,
    pub attachments: Vec<crate::attachments::Attachment>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Member {
    pub member: String,
    pub kind: String,
}

const ROOM_COLS: &str =
    "id, name, state, frozen, folder, pinned, created_by, created_at, deleted_at, deleted_by";

fn room_row(r: &rusqlite::Row) -> rusqlite::Result<Room> {
    Ok(Room {
        id: r.get(0)?,
        name: r.get(1)?,
        state: r.get(2)?,
        frozen: r.get::<_, i64>(3)? != 0,
        folder: r.get(4)?,
        pinned: r.get::<_, i64>(5)? != 0,
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        deleted_at: r.get(8)?,
        deleted_by: r.get(9)?,
    })
}

const MSG_COLS: &str = "id, room, author, author_kind, body, reply_to, thread_id, created_at";

fn msg_row(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        room: r.get(1)?,
        author: r.get(2)?,
        author_kind: r.get(3)?,
        body: r.get(4)?,
        reply_to: r.get(5)?,
        thread_id: r.get(6)?,
        created_at: r.get(7)?,
        attachments: vec![],
    })
}

pub fn room(tx: &rusqlite::Connection, id: i64) -> Result<Option<Room>> {
    Ok(tx
        .query_row(
            &format!("SELECT {ROOM_COLS} FROM chat_rooms WHERE id = ?1"),
            [id],
            room_row,
        )
        .optional()?)
}

pub fn is_member(tx: &rusqlite::Connection, room: i64, who: &str) -> Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM chat_members WHERE room = ?1 AND member = ?2",
            params![room, who],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn members(tx: &rusqlite::Connection, room: i64) -> Result<Vec<Member>> {
    let mut st = tx.prepare(
        "SELECT member, kind FROM chat_members WHERE room = ?1 ORDER BY kind DESC, member",
    )?;
    let v = st
        .query_map([room], |r| {
            Ok(Member {
                member: r.get(0)?,
                kind: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(v)
}

/// 每群未讀數（作者不是自己、id 大於已讀游標）與提及數。
pub fn unread_counts(db: &Db, viewer: &str) -> Result<Vec<(i64, i64, i64)>> {
    db.with(|c| {
        let mut st = c.prepare(
            "SELECT m.room,
                    count(*),
                    sum(CASE WHEN instr(m.body, '@' || ?1) > 0 THEN 1 ELSE 0 END)
             FROM chat_messages m
             JOIN chat_members b ON b.room = m.room AND b.member = ?1
             LEFT JOIN chat_reads r ON r.room = m.room AND r.member = ?1
             WHERE m.author != ?1 AND m.id > COALESCE(r.through, 0)
             GROUP BY m.room",
        )?;
        let v = st
            .query_map([viewer], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    })
}

/// 已讀游標只往前移。
pub fn mark_read(db: &Db, room: i64, viewer: &str, through: i64) -> Result<()> {
    db.with(|c| {
        c.execute(
            "INSERT INTO chat_reads(room, member, through) VALUES (?1, ?2, ?3)
             ON CONFLICT(room, member) DO UPDATE SET through = max(through, excluded.through)",
            params![room, viewer, through],
        )?;
        Ok(())
    })
}

pub fn list_rooms(db: &Db, viewer: &str, see_all: bool, state: &str) -> Result<Vec<Room>> {
    db.with(|c| {
        let sql = format!(
            "SELECT {ROOM_COLS} FROM chat_rooms r WHERE state = ?1 AND (?2 OR EXISTS (SELECT 1 FROM chat_members m WHERE m.room = r.id AND m.member = ?3))
             ORDER BY pinned DESC, id DESC"
        );
        let mut st = c.prepare(&sql)?;
        let v = st.query_map(params![state, see_all, viewer], room_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    })
}

/// 成員種類：人類帳號（auth_users）或角色（roles.json）。
pub fn member_kind(
    tx: &rusqlite::Connection,
    roster: &Roster,
    name: &str,
) -> Result<Option<&'static str>> {
    if tx
        .query_row("SELECT 1 FROM auth_users WHERE name = ?1", [name], |_| {
            Ok(())
        })
        .optional()?
        .is_some()
    {
        return Ok(Some("user"));
    }
    Ok(roster.roles.contains_key(name).then_some("role"))
}

fn audit(tx: &Transaction, room: &Room, action: &str, actor: &str, detail: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO chat_room_audit(room, room_name, action, actor, detail, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![room.id, room.name, action, actor, detail, now()],
    )?;
    Ok(())
}

/// 在交易內執行 f，commit 後依序廣播 f 記錄的事件。
pub fn write<R>(
    core: &Core,
    f: impl FnOnce(&Transaction, &mut Vec<EventEnvelope>) -> Result<R>,
) -> Result<R> {
    let (r, evs) = core.db.with(|c| {
        let tx = c.transaction()?;
        let mut evs = Vec::new();
        let r = f(&tx, &mut evs)?;
        tx.commit()?;
        Ok((r, evs))
    })?;
    for e in evs {
        core.bus.announce(e);
    }
    Ok(r)
}

pub fn validate_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 80 {
        return Err(Error::Other("room name must be 1-80 characters".into()));
    }
    Ok(n.to_string())
}

pub fn create_room(
    tx: &Transaction,
    evs: &mut Vec<EventEnvelope>,
    roster: &Roster,
    name: &str,
    creator: &str,
    members_: &[String],
) -> Result<Room> {
    let name = validate_name(name)?;
    tx.execute(
        "INSERT INTO chat_rooms(name, created_by, created_at) VALUES (?1, ?2, ?3)",
        params![name, creator, now()],
    )?;
    let id = tx.last_insert_rowid();
    let mut all: Vec<&str> = vec![creator];
    all.extend(members_.iter().map(String::as_str));
    for m in all {
        add_member_tx(tx, roster, id, m)?;
    }
    let r = room(tx, id)?.expect("just inserted");
    audit(tx, &r, "created", creator, "")?;
    evs.push(EventBus::record(
        tx,
        "chat.room.created",
        1,
        json!({"room": r, "members": members(tx, id)?}),
    )?);
    Ok(r)
}

pub fn add_member_tx(tx: &Transaction, roster: &Roster, room: i64, who: &str) -> Result<bool> {
    let kind = member_kind(tx, roster, who)?
        .ok_or_else(|| Error::Other(format!("unknown member: {who}")))?;
    Ok(tx.execute(
        "INSERT OR IGNORE INTO chat_members(room, member, kind, added_at) VALUES (?1, ?2, ?3, ?4)",
        params![room, who, kind, now()],
    )? > 0)
}

/// 移除成員：刪成員列、取消這人在本群還沒投遞的信、若是本群 lead 一併撤換，公告並記稽核。
/// 歷史訊息保留。回傳是否真的移除（本來就不在群裡回 false）。
pub fn remove_member_tx(
    tx: &Transaction,
    evs: &mut Vec<EventEnvelope>,
    roster: &Roster,
    room_id: i64,
    by: &str,
    member: &str,
) -> Result<bool> {
    let Some(r) = room(tx, room_id)? else {
        return Err(Error::Other(format!("unknown room: {room_id}")));
    };
    if tx.execute(
        "DELETE FROM chat_members WHERE room = ?1 AND member = ?2",
        params![room_id, member],
    )? == 0
    {
        return Ok(false);
    }
    tx.execute(
        "UPDATE chat_deliveries SET state = 'cancelled' WHERE recipient = ?1 AND state = 'queued'
         AND message IN (SELECT id FROM chat_messages WHERE room = ?2)",
        params![member, room_id],
    )?;
    let was_lead = tx.execute(
        "DELETE FROM chat_room_leads WHERE room = ?1 AND role = ?2",
        params![room_id, member],
    )? > 0;
    audit(tx, &r, "member_removed", by, member)?;
    post_system(
        tx,
        evs,
        roster,
        room_id,
        &format!("{by} removed {member} from the room."),
    )?;
    evs.push(EventBus::record(
        tx,
        "chat.member.removed",
        1,
        json!({"room": room_id, "member": member, "by": by}),
    )?);
    if was_lead {
        evs.push(EventBus::record(
            tx,
            "chat.room.updated",
            1,
            json!({"room": r, "lead": null}),
        )?);
    }
    Ok(true)
}

/// 系統公告：author_kind=system、不喚醒任何角色（凍結、核准、成員變動等）。
pub fn post_system(
    tx: &Transaction,
    evs: &mut Vec<EventEnvelope>,
    roster: &Roster,
    room: i64,
    body: &str,
) -> Result<Message> {
    let (m, _) = post_message_opts(
        tx,
        evs,
        roster,
        room,
        "system",
        "system",
        body,
        None,
        false,
        &[],
    )?;
    Ok(m)
}

/// 「X added A, B to the room.」：同一人 60 秒內連續邀人，且那則仍是群裡最新一則時，
/// 併進同一則系統訊息（改 body、發 chat.message.updated），不再一人一行。
pub fn post_member_added(
    tx: &Transaction,
    evs: &mut Vec<EventEnvelope>,
    roster: &Roster,
    room: i64,
    by: &str,
    member: &str,
) -> Result<Message> {
    const WINDOW: i64 = 60;
    let prefix = format!("{by} added ");
    let suffix = " to the room.";
    let last: Option<(i64, String, String, i64)> = tx
        .query_row(
            "SELECT id, author_kind, body, created_at FROM chat_messages WHERE room = ?1 ORDER BY id DESC LIMIT 1",
            params![room],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((mid, kind, body, at)) = last {
        if kind == "system"
            && now() - at <= WINDOW
            && body.starts_with(&prefix)
            && body.ends_with(suffix)
        {
            let names = &body[prefix.len()..body.len() - suffix.len()];
            let merged = format!("{prefix}{names}, {member}{suffix}");
            tx.execute(
                "UPDATE chat_messages SET body = ?1 WHERE id = ?2",
                params![merged, mid],
            )?;
            let m = tx.query_row(
                &format!("SELECT {MSG_COLS} FROM chat_messages WHERE id = ?1"),
                [mid],
                msg_row,
            )?;
            evs.push(EventBus::record(
                tx,
                "chat.message.updated",
                1,
                json!({"message": m}),
            )?);
            return Ok(m);
        }
    }
    post_system(tx, evs, roster, room, &format!("{prefix}{member}{suffix}"))
}

/// 群的資料夾清單（有群在用的；依名稱排序）。
pub fn folders(c: &rusqlite::Connection) -> Result<Vec<(String, i64)>> {
    let mut st = c.prepare("SELECT folder, count(*) FROM chat_rooms WHERE folder != '' AND state IN ('active','archived') GROUP BY folder ORDER BY folder")?;
    let v = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(v)
}

/// 從內文取出 @提及（@ 後接名稱字元）。
pub fn mentions(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in body.match_indices('@') {
        // email：@ 前緊貼一段 local-part（英數字與 . + - _ %），且其中有英數字 → 不是提及。
        // 只有標點沒有英數字（例如 Markdown 粗體 `**@maker**`）仍算提及。
        let local: Vec<char> = body[..i]
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '+' | '-' | '_' | '%'))
            .collect();
        if local.iter().any(|c| c.is_alphanumeric()) {
            continue;
        }
        let name: String = body[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        // 名稱後緊接「.英數字」是網域（@reviewer.example）→ 不是提及；句尾的「@maker.」仍算
        let rest = &body[i + 1 + name.len()..];
        let mut it = rest.chars();
        if it.next() == Some('.') && it.next().is_some_and(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// 發訊息（人或角色）。回傳訊息與被投遞的角色。
#[allow(clippy::too_many_arguments)]
pub fn post_message(
    tx: &Transaction,
    evs: &mut Vec<EventEnvelope>,
    roster: &Roster,
    room_id: i64,
    author: &str,
    author_kind: &str,
    body: &str,
    reply_to: Option<i64>,
) -> Result<(Message, Vec<String>)> {
    post_message_opts(
        tx,
        evs,
        roster,
        room_id,
        author,
        author_kind,
        body,
        reply_to,
        true,
        &[],
    )
}

/// `broadcast`=false：只投遞給被 @ 的角色（系統通知，例如任務狀態變更），不因「人發言」喚醒全群。
#[allow(clippy::too_many_arguments)]
pub fn post_message_opts(
    tx: &Transaction,
    evs: &mut Vec<EventEnvelope>,
    roster: &Roster,
    room_id: i64,
    author: &str,
    author_kind: &str,
    body: &str,
    reply_to: Option<i64>,
    broadcast: bool,
    attachments: &[i64],
) -> Result<(Message, Vec<String>)> {
    let body = body.trim();
    // 只有附件時內文可空白
    if (body.is_empty() && attachments.is_empty()) || body.len() > MAX_BODY {
        return Err(Error::Other(format!(
            "message body must be 1-{MAX_BODY} bytes"
        )));
    }
    let r = room(tx, room_id)?.ok_or_else(|| Error::Other("room not found".into()))?;
    if r.state != "active" {
        return Err(Error::Other(format!("room is {}", r.state)));
    }
    let thread_id = match reply_to {
        Some(p) => {
            let parent: Option<(i64, Option<i64>)> = tx
                .query_row(
                    "SELECT room, thread_id FROM chat_messages WHERE id = ?1",
                    [p],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match parent {
                Some((pr, t)) if pr == room_id => Some(t.unwrap_or(p)),
                _ => {
                    return Err(Error::Other(
                        "reply_to must be a message in the same room".into(),
                    ))
                }
            }
        }
        None => None,
    };
    tx.execute(
        "INSERT INTO chat_messages(room, author, author_kind, body, reply_to, thread_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![room_id, author, author_kind, body, reply_to, thread_id, now()],
    )?;
    let id = tx.last_insert_rowid();
    let bound = crate::attachments::bind(tx, room_id, author, id, attachments)?;
    let role_members: Vec<String> = members(tx, room_id)?
        .into_iter()
        .filter(|m| m.kind == "role" && m.member != author)
        .filter(|m| {
            roster
                .roles
                .get(&m.member)
                .is_some_and(|r| !r.driver.is_manual())
        })
        .map(|m| m.member)
        .collect();
    let mentioned: Vec<String> = mentions(body)
        .into_iter()
        .filter(|m| role_members.contains(m))
        .collect();
    let mut recipients = if !broadcast || author_kind == "role" || !mentioned.is_empty() {
        mentioned
    } else {
        role_members.clone()
    };
    // lead 掌握全群：沒 @ 它的發言也送給它（它自己判斷要不要回）；系統通知不算
    if broadcast
        && author_kind != "system"
        && LEAD_SEES_ALL.load(std::sync::atomic::Ordering::Relaxed)
    {
        if let Some(l) = notice_lead(tx, roster, room_id)? {
            if l != author && role_members.contains(&l) && !recipients.contains(&l) {
                recipients.push(l);
            }
        }
    }
    let limit = AGENT_CHAIN_LIMIT.load(std::sync::atomic::Ordering::Relaxed);
    let mut paused = false;
    if author_kind == "role" && limit > 0 {
        let chain: usize = tx.query_row(
            "SELECT count(*) FROM chat_messages WHERE room = ?1 AND author_kind = 'role'
               AND id > coalesce((SELECT max(id) FROM chat_messages WHERE room = ?1 AND author_kind = 'user'), 0)",
            [room_id],
            |r| r.get::<_, i64>(0),
        )? as usize;
        if chain > limit {
            recipients.clear();
            paused = chain == limit + 1;
        }
    }
    for to in &recipients {
        tx.execute(
            "INSERT INTO chat_deliveries(message, recipient) VALUES (?1, ?2)",
            params![id, to],
        )?;
    }
    let mut m = tx.query_row(
        &format!("SELECT {MSG_COLS} FROM chat_messages WHERE id = ?1"),
        [id],
        msg_row,
    )?;
    m.attachments = bound;
    evs.push(EventBus::record(
        tx,
        "chat.message.created",
        1,
        json!({"message": m, "delivered_to": recipients}),
    )?);
    if author_kind != "system" {
        notify_undelivered(tx, evs, roster, room_id, author, author_kind, body)?;
    }
    if paused {
        post_system(
            tx,
            evs,
            roster,
            room_id,
            &format!("Agents paused: {limit} agent messages in a row with no human reply, so agents no longer wake each other here. Post a message to resume."),
        )?;
    }
    Ok((m, recipients))
}

/// @ 到 roles.json 有、但不在本群的角色：貼系統通知說沒送達。
/// 作者是角色時通知帶 @作者叫醒他改派；同群 10 分鐘內同內文只貼一次。
fn notify_undelivered(
    tx: &Transaction,
    evs: &mut Vec<EventEnvelope>,
    roster: &Roster,
    room_id: i64,
    author: &str,
    author_kind: &str,
    body: &str,
) -> Result<()> {
    const WINDOW: i64 = 600;
    let mut outside = Vec::new();
    for name in mentions(body) {
        if name != author && roster.roles.contains_key(&name) && !is_member(tx, room_id, &name)? {
            outside.push(name);
        }
    }
    if outside.is_empty() {
        return Ok(());
    }
    let names = outside.join("、");
    let text = if author_kind == "role" {
        format!("@{author} 你提到的 {names} 不在本群，這則沒有送給他們。請改派本群成員，或請群主（owner）邀請。")
    } else {
        format!("你提到的 {names} 不在本群，這則沒有送給他們。")
    };
    let dup: bool = tx
        .query_row(
            "SELECT 1 FROM chat_messages WHERE room = ?1 AND author_kind = 'system' AND body = ?2 AND created_at >= ?3",
            params![room_id, text, now() - WINDOW],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !dup {
        post_system(tx, evs, roster, room_id, &text)?;
    }
    Ok(())
}

pub fn messages(db: &Db, room: i64, after: i64, limit: i64) -> Result<Vec<Message>> {
    db.with(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {MSG_COLS} FROM chat_messages WHERE room = ?1 AND id > ?2 ORDER BY id LIMIT ?3"
        ))?;
        let mut v: Vec<Message> = st
            .query_map(params![room, after, limit.clamp(1, 500)], msg_row)?
            .collect::<rusqlite::Result<_>>()?;
        let ids: Vec<i64> = v.iter().map(|m| m.id).collect();
        let mut att = crate::attachments::for_messages(c, &ids)?;
        for m in &mut v {
            m.attachments = att.remove(&m.id).unwrap_or_default();
        }
        Ok(v)
    })
}

/// 最新的 `limit` 則（由舊到新排列）。開群時用，避免訊息多時只看到最舊的。
pub fn messages_latest(db: &Db, room: i64, limit: i64) -> Result<Vec<Message>> {
    let limit = limit.clamp(1, 500);
    let first: i64 = db.with(|c| {
        Ok(c.query_row(
            "SELECT COALESCE(MIN(id), 0) FROM (SELECT id FROM chat_messages WHERE room = ?1 ORDER BY id DESC LIMIT ?2)",
            params![room, limit],
            |r| r.get(0),
        )?)
    })?;
    messages(db, room, (first - 1).max(0), limit)
}

/// 以某則訊息為中心取前後各 `half` 則（跳到訊息位置用）。
pub fn messages_around(db: &Db, room: i64, center: i64, half: i64) -> Result<Vec<Message>> {
    let half = half.clamp(1, 250);
    let first: i64 = db.with(|c| {
        Ok(c.query_row(
            "SELECT COALESCE(MIN(id), ?2) FROM (SELECT id FROM chat_messages WHERE room = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3)",
            params![room, center, half],
            |r| r.get(0),
        )?)
    })?;
    messages(db, room, first - 1, half * 2 + 1)
}

/// 改變群狀態：archive／delete（進垃圾桶）／restore。非 active 時取消待投遞的信。
pub fn set_state(core: &Core, room_id: i64, action: &str, actor: &str) -> Result<Room> {
    write(core, |tx, evs| {
        let r = room(tx, room_id)?.ok_or_else(|| Error::Other("room not found".into()))?;
        let (state, deleted_at, deleted_by): (&str, Option<i64>, Option<&str>) =
            match (action, r.state.as_str()) {
                ("archive", "active") => ("archived", None, None),
                ("delete", "active" | "archived") => ("deleted", Some(now()), Some(actor)),
                ("restore", "archived" | "deleted") => ("active", None, None),
                _ => {
                    return Err(Error::Other(format!(
                        "cannot {action} a room that is {}",
                        r.state
                    )))
                }
            };
        tx.execute(
            "UPDATE chat_rooms SET state = ?1, deleted_at = ?2, deleted_by = ?3 WHERE id = ?4",
            params![state, deleted_at, deleted_by, room_id],
        )?;
        if state != "active" {
            tx.execute(
                "UPDATE chat_deliveries SET state = 'cancelled' WHERE state = 'queued' AND message IN (SELECT id FROM chat_messages WHERE room = ?1)",
                [room_id],
            )?;
        }
        audit(tx, &r, action, actor, "")?;
        let nr = room(tx, room_id)?.expect("exists");
        let kind = match action {
            "archive" => "chat.room.archived",
            "delete" => "chat.room.deleted",
            _ => "chat.room.restored",
        };
        evs.push(EventBus::record(
            tx,
            kind,
            1,
            json!({"room": nr, "by": actor}),
        )?);
        Ok(nr)
    })
}

/// 永久刪除（僅垃圾桶中的群）：先關閉並遺忘所有 lane，再刪資料；稽核紀錄保留。
pub async fn purge(core: &Core, disp: &Dispatcher, room_id: i64, actor: &str) -> Result<()> {
    if disp.busy_in_room(room_id) {
        return Err(Error::Other(
            "room has a running turn; stop it first".into(),
        ));
    }
    {
        let r = core
            .db
            .with(|c| room(c, room_id))?
            .ok_or_else(|| Error::Other("room not found".into()))?;
        if r.state != "deleted" {
            return Err(Error::Other("only rooms in the trash can be purged".into()));
        }
    }
    disp.close_room(room_id, true).await?;
    write(core, |tx, evs| {
        let r = room(tx, room_id)?.ok_or_else(|| Error::Other("room not found".into()))?;
        let n: i64 = tx.query_row(
            "SELECT count(*) FROM chat_messages WHERE room = ?1",
            [room_id],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM chat_sessions WHERE room = ?1", [room_id])?;
        tx.execute("DELETE FROM chat_rooms WHERE id = ?1", [room_id])?; // members/messages/deliveries/reads 由 CASCADE 刪除
        audit(tx, &r, "purged", actor, &format!("{n} messages"))?;
        evs.push(EventBus::record(
            tx,
            "chat.room.purged",
            1,
            json!({"room": room_id, "name": r.name, "by": actor, "messages": n}),
        )?);
        Ok(())
    })
}

/// 垃圾桶自動清空（AAF_CHAT_TRASH_RETENTION_DAYS；0＝關閉）。進行中的群跳過，下次再試。
pub async fn purge_expired(core: &Core, disp: &Dispatcher) -> Result<Vec<i64>> {
    let days: i64 = core.config.parse("AAF_CHAT_TRASH_RETENTION_DAYS")?;
    if days <= 0 {
        return Ok(vec![]);
    }
    let cutoff = now() - days * 86400;
    let ids: Vec<i64> = core.db.with(|c| {
        let mut st =
            c.prepare("SELECT id FROM chat_rooms WHERE state = 'deleted' AND deleted_at <= ?1")?;
        let v = st
            .query_map([cutoff], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    })?;
    let mut done = Vec::new();
    for id in ids {
        match purge(core, disp, id, "system:retention").await {
            Ok(()) => done.push(id),
            Err(e) => tracing::warn!(room = id, error = %e, "retention purge skipped"),
        }
    }
    Ok(done)
}

pub fn audit_log(db: &Db, room: i64) -> Result<Vec<serde_json::Value>> {
    db.with(|c| {
        let mut st = c.prepare("SELECT action, actor, detail, at FROM chat_room_audit WHERE room = ?1 ORDER BY id")?;
        let v = st
            .query_map([room], |r| Ok(json!({"action": r.get::<_, String>(0)?, "actor": r.get::<_, String>(1)?, "detail": r.get::<_, String>(2)?, "at": r.get::<_, i64>(3)?})))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    })
}

/// 看得到的群 id（未刪除）。Owner 全部。
pub fn visible_room_ids(db: &Db, viewer: &str, see_all: bool) -> Result<Vec<i64>> {
    db.with(|c| {
        let mut st = c.prepare(
            "SELECT id FROM chat_rooms r WHERE state != 'deleted' AND (?1 OR EXISTS (SELECT 1 FROM chat_members m WHERE m.room = r.id AND m.member = ?2)) ORDER BY id",
        )?;
        let v = st.query_map(params![see_all, viewer], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(v)
    })
}

/// 各群待投遞（尚未被角色讀取）的訊息數：(群 id, 群名, 數量)。
pub fn queued_by_room(db: &Db) -> Result<Vec<(i64, String, i64)>> {
    db.with(|c| {
        let mut st = c.prepare(
            "SELECT r.id, r.name, COUNT(*) FROM chat_deliveries d JOIN chat_messages m ON m.id = d.message JOIN chat_rooms r ON r.id = m.room
             WHERE d.state = 'queued' AND r.state = 'active' GROUP BY r.id ORDER BY 3 DESC",
        )?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}

// ---------------- 群級 lead ----------------

/// 本群指定的 lead（沒指定回 None）。
pub fn room_lead(c: &rusqlite::Connection, room: i64) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT role FROM chat_room_leads WHERE room = ?1",
        [room],
        |r| r.get(0),
    )
    .optional()?)
}

/// 這個角色在這個群算不算 lead：群有指定 → 只認指定者（且須仍是本群成員）；
/// 沒指定 → 退回全域 rank。
pub fn is_lead_in(c: &rusqlite::Connection, roster: &Roster, room: i64, who: &str) -> Result<bool> {
    if !is_member(c, room, who)? {
        return Ok(false);
    }
    Ok(match room_lead(c, room)? {
        Some(l) => l == who,
        None => roster.roles.get(who).is_some_and(|r| r.rank == "lead"),
    })
}

/// 失敗公告要 @ 的帶隊：群有指定 lead 且仍是成員 → 它；否則本群 rank=lead 的非手動角色（字母序第一）。
pub fn notice_lead(c: &rusqlite::Connection, roster: &Roster, room: i64) -> Result<Option<String>> {
    let roles: Vec<String> = members(c, room)?
        .into_iter()
        .filter(|m| m.kind == "role")
        .map(|m| m.member)
        .collect();
    if let Some(l) = room_lead(c, room)? {
        return Ok(roles.contains(&l).then_some(l));
    }
    let mut leads: Vec<String> = roles
        .into_iter()
        .filter(|r| {
            roster
                .roles
                .get(r)
                .is_some_and(|s| s.rank == "lead" && !s.driver.is_manual())
        })
        .collect();
    leads.sort();
    Ok(leads.into_iter().next())
}

/// owner 指定／撤換本群 lead；貼系統訊息、發 chat.room.updated。回傳新的 lead。
pub fn set_room_lead(
    tx: &Transaction,
    evs: &mut Vec<EventEnvelope>,
    roster: &Roster,
    room_id: i64,
    role: Option<&str>,
    by: &str,
) -> Result<Option<String>> {
    let r = room(tx, room_id)?.ok_or_else(|| Error::Other("room not found".into()))?;
    if r.state != "active" {
        return Err(Error::Other(format!("room is {}", r.state)));
    }
    let prev = room_lead(tx, room_id)?;
    if prev.as_deref() == role {
        return Ok(prev);
    }
    let body = match role {
        Some(name) => {
            if !roster.roles.contains_key(name) {
                return Err(Error::Other(format!("unknown role: {name}")));
            }
            if !members(tx, room_id)?
                .iter()
                .any(|m| m.kind == "role" && m.member == name)
            {
                return Err(Error::Other(format!(
                    "{name} is not a member of this room; invite them first"
                )));
            }
            tx.execute(
                "INSERT INTO chat_room_leads(room, role, by, at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(room) DO UPDATE SET role = excluded.role, by = excluded.by, at = excluded.at",
                params![room_id, name, by, now()],
            )?;
            format!("{by} 指定 {name} 為本群 lead。")
        }
        None => {
            tx.execute("DELETE FROM chat_room_leads WHERE room = ?1", [room_id])?;
            format!(
                "{by} 撤換本群 lead（原為 {}），改回依全域設定。",
                prev.as_deref().unwrap_or("-")
            )
        }
    };
    audit(tx, &r, "lead", by, role.unwrap_or(""))?;
    post_system(tx, evs, roster, room_id, &body)?;
    evs.push(EventBus::record(
        tx,
        "chat.room.updated",
        1,
        json!({"room": r, "lead": role}),
    )?);
    Ok(role.map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn parses_mentions() {
        assert_eq!(
            mentions("@coder please, cc @rev-1 and a@b.com @coder"),
            vec!["coder", "rev-1"]
        );
        assert!(mentions("no mentions @").is_empty());
        // email 的 local-part 標點與網域都不能被當成提及
        assert!(mentions("聯絡信箱 ops+@reviewer.example，這不是派工。").is_empty());
        assert!(mentions("a.b@coder.dev x_y@coder.io a%b@coder.tw").is_empty());
        assert!(mentions("寫信到 @reviewer.example").is_empty());
        // 句尾標點仍算提及
        assert_eq!(mentions("交給 @maker. 謝謝"), vec!["maker"]);
        assert_eq!(
            mentions("交給 @maker。@auditor，請驗"),
            vec!["maker", "auditor"]
        );
        assert_eq!(mentions("(@coder)"), vec!["coder"]);
        // Markdown 粗體仍算提及
        assert_eq!(mentions("**@reviewer** 請確認"), vec!["reviewer"]);
        assert_eq!(mentions("**@maker**、*@auditor*"), vec!["maker", "auditor"]);
    }
}
