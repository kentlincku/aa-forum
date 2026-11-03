//! 任務板（源自先前的 Python 版）：每個任務屬於一個群。
//!
//! 狀態：open → claimed（有租約）→ done／blocked；發起人可 cancelled。
//! - 建立：貼一則任務訊息到群裡；有指派就 @提及對象（喚醒該角色），否則 @all 角色。
//! - 認領：open 可認領；自己 claimed 可續租；blocked 時原 assignee 或發起人可重新認領（發起人＝接手）。
//! - 指派：assignee 先寫入；open 且已指派時只有被指派者或發起人（＝接手）可認領。
//! - 租約到期未回報：回到 open 並清除指派，任何成員可再認領（背景工作 + 每次讀取時檢查）。
//! - 狀態通知只投遞給被 @ 的角色，不廣播喚醒全群。
//! - 回報 done／blocked：須是當前 assignee 且 claimed；在群裡回覆任務訊息並 @發起人。
//! - 群非 active 時不可建立或變更任務。

use crate::store::post_system;
use crate::store::{self, now, post_message, post_message_opts, room, write};
use aaf_core::events::EventBus;
use aaf_core::{Core, Error, Result};
use aaf_runtime::dispatcher::Roster;
use rusqlite::{params, OptionalExtension, Transaction};
use serde::Serialize;
use serde_json::json;

pub const DEFAULT_LEASE_SECS: i64 = 1800;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Task {
    pub id: i64,
    pub room: i64,
    pub title: String,
    pub spec: String,
    pub owner: String,
    pub assignee: Option<String>,
    pub state: String,
    pub lease_until: Option<i64>,
    pub result: String,
    pub message: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

const COLS: &str = "id, room, title, spec, owner, assignee, state, lease_until, result, message, created_at, updated_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        room: r.get(1)?,
        title: r.get(2)?,
        spec: r.get(3)?,
        owner: r.get(4)?,
        assignee: r.get(5)?,
        state: r.get(6)?,
        lease_until: r.get(7)?,
        result: r.get(8)?,
        message: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

pub fn get(c: &rusqlite::Connection, id: i64) -> Result<Option<Task>> {
    Ok(c.query_row(
        &format!("SELECT {COLS} FROM chat_tasks WHERE id = ?1"),
        [id],
        row,
    )
    .optional()?)
}

/// 到期的 claimed 任務回到 open。回傳被釋放的任務 id。
pub fn expire(tx: &Transaction, evs: &mut Vec<aaf_contract::EventEnvelope>) -> Result<Vec<i64>> {
    let ids: Vec<i64> = {
        let mut st =
            tx.prepare("SELECT id FROM chat_tasks WHERE state = 'claimed' AND lease_until < ?1")?;
        let v = st
            .query_map([now()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        v
    };
    for id in &ids {
        // 租約到期不算進度：不動 updated_at（停滯清單只認任務本身的更新）。
        tx.execute(
            "UPDATE chat_tasks SET state = 'open', assignee = NULL, lease_until = NULL WHERE id = ?1",
            params![id],
        )?;
        let t = get(tx, *id)?.expect("exists");
        evs.push(EventBus::record(
            tx,
            "chat.task.updated",
            1,
            json!({"task": t, "reason": "lease_expired"}),
        )?);
    }
    Ok(ids)
}

pub fn list(core: &Core, room_id: i64, state: Option<&str>) -> Result<Vec<Task>> {
    write(core, |tx, evs| {
        expire(tx, evs)?;
        let mut st = tx.prepare(&format!("SELECT {COLS} FROM chat_tasks WHERE room = ?1 AND (?2 IS NULL OR state = ?2) ORDER BY id"))?;
        let v = st
            .query_map(params![room_id, state], row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    })
}

/// 任務可見範圍：查詢者所屬群的任務（mine＝我發起或指派給我）。
pub fn list_for(core: &Core, who: &str, mine: bool, state: Option<&str>) -> Result<Vec<Task>> {
    write(core, |tx, evs| {
        expire(tx, evs)?;
        let mut st = tx.prepare(&format!(
            "SELECT {COLS} FROM chat_tasks t WHERE EXISTS (SELECT 1 FROM chat_members m WHERE m.room = t.room AND m.member = ?1)
             AND (?2 = 0 OR t.owner = ?1 OR t.assignee = ?1) AND (?3 IS NULL OR t.state = ?3) ORDER BY id"
        ))?;
        let v = st
            .query_map(params![who, mine, state], row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    })
}

fn active_room(tx: &Transaction, room_id: i64) -> Result<()> {
    let r = room(tx, room_id)?.ok_or_else(|| Error::Other("room not found".into()))?;
    if r.state != "active" {
        return Err(Error::Other(format!("room is {}", r.state)));
    }
    Ok(())
}

fn author_kind(roster: &Roster, who: &str) -> &'static str {
    if roster.roles.contains_key(who) {
        "role"
    } else {
        "user"
    }
}

fn update_event(
    tx: &Transaction,
    evs: &mut Vec<aaf_contract::EventEnvelope>,
    id: i64,
    by: &str,
) -> Result<Task> {
    let t = get(tx, id)?.expect("exists");
    evs.push(EventBus::record(
        tx,
        "chat.task.updated",
        1,
        json!({"task": t, "by": by}),
    )?);
    Ok(t)
}

pub fn create(
    core: &Core,
    roster: &Roster,
    room_id: i64,
    owner: &str,
    title: &str,
    spec: &str,
    assignee: Option<&str>,
) -> Result<Task> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(Error::Other("title must be 1-200 characters".into()));
    }
    write(core, |tx, evs| {
        active_room(tx, room_id)?;
        if let Some(a) = assignee {
            if !store::is_member(tx, room_id, a)? {
                return Err(Error::Other(format!(
                    "{a} is not a member of this room; invite them first"
                )));
            }
        }
        let t = now();
        tx.execute(
            "INSERT INTO chat_tasks(room, title, spec, owner, assignee, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![room_id, title, spec.trim(), owner, assignee, t],
        )?;
        let id = tx.last_insert_rowid();
        let to = assignee.map(|a| format!("@{a} ")).unwrap_or_default();
        let body = format!("{to}[task #{id}] {title}\n\n{}\n\n(claim: `aaf task claim {id}` · done: `aaf task done {id} \"result\"`)", spec.trim())
            .replace("\n\n\n\n", "\n\n");
        let (m, _) = post_message(
            tx,
            evs,
            roster,
            room_id,
            owner,
            author_kind(roster, owner),
            &body,
            None,
        )?;
        tx.execute(
            "UPDATE chat_messages SET task_id = ?1 WHERE id = ?2",
            params![id, m.id],
        )?;
        tx.execute(
            "UPDATE chat_tasks SET message = ?1 WHERE id = ?2",
            params![m.id, id],
        )?;
        let task = get(tx, id)?.expect("exists");
        evs.push(EventBus::record(
            tx,
            "chat.task.created",
            1,
            json!({"task": task, "assignee": assignee}),
        )?);
        Ok(task)
    })
}

pub fn claim(core: &Core, roster: &Roster, id: i64, who: &str, lease_secs: i64) -> Result<Task> {
    write(core, |tx, evs| {
        expire(tx, evs)?;
        let t = get(tx, id)?.ok_or_else(|| Error::Other("task not found".into()))?;
        active_room(tx, t.room)?;
        if !store::is_member(tx, t.room, who)? {
            return Err(Error::Denied("not a member of the task's room".into()));
        }
        let mut takeover = None;
        match t.state.as_str() {
            "open" => match t.assignee.as_deref() {
                Some(a) if a != who && t.owner != who => {
                    return Err(Error::Denied(format!(
                        "assigned to {a}; only they or the owner can claim"
                    )))
                }
                Some(a) if a != who => takeover = Some(a.to_string()),
                _ => {}
            },
            "claimed" if t.assignee.as_deref() == Some(who) => {} // 續租
            "blocked" if t.assignee.as_deref() == Some(who) => {}
            "blocked" if t.owner == who => takeover = t.assignee.clone(),
            "blocked" => {
                return Err(Error::Denied(
                    "blocked: only the assignee or the owner can reclaim".into(),
                ))
            }
            s => {
                return Err(Error::Other(format!(
                    "task is {s} ({})",
                    t.assignee.as_deref().unwrap_or("-")
                )))
            }
        }
        tx.execute(
            "UPDATE chat_tasks SET state = 'claimed', assignee = ?1, lease_until = ?2, updated_at = ?3 WHERE id = ?4",
            params![who, now() + lease_secs, now(), id],
        )?;
        if let Some(prev) = takeover {
            let body = format!(
                "@{prev} [task #{id}] taken over by {who}; you no longer need to work on it."
            );
            post_message_opts(
                tx,
                evs,
                roster,
                t.room,
                who,
                author_kind(roster, who),
                &body,
                t.message,
                false,
                &[],
            )?;
        }
        update_event(tx, evs, id, who)
    })
}

/// 回報 done／blocked（assignee），或 cancelled（發起人）。
pub fn report(
    core: &Core,
    roster: &Roster,
    id: i64,
    who: &str,
    state: &str,
    result: &str,
) -> Result<Task> {
    if !["done", "blocked", "cancelled"].contains(&state) {
        return Err(Error::Other(
            "state must be done, blocked or cancelled".into(),
        ));
    }
    write(core, |tx, evs| {
        expire(tx, evs)?;
        let t = get(tx, id)?.ok_or_else(|| Error::Other("task not found".into()))?;
        active_room(tx, t.room)?;
        if state == "cancelled" {
            if t.owner != who {
                return Err(Error::Denied("only the owner can cancel".into()));
            }
            if t.state == "done" || t.state == "cancelled" {
                return Err(Error::Other(format!("task is already {}", t.state)));
            }
        } else if t.assignee.as_deref() != Some(who) || t.state != "claimed" {
            return Err(Error::Denied("claim the task before reporting".into()));
        }
        tx.execute(
            "UPDATE chat_tasks SET state = ?1, result = ?2, lease_until = NULL, updated_at = ?3 WHERE id = ?4",
            params![state, result.trim(), now(), id],
        )?;
        // 回覆任務訊息；回報給發起人（角色發的訊息只喚醒被 @ 的角色，人類發起人看群即可）
        let mention = if who != t.owner {
            format!("@{} ", t.owner)
        } else {
            String::new()
        };
        let body = format!("{mention}[task #{id} → {state}] {}", result.trim())
            .trim_end()
            .to_string();
        post_message_opts(
            tx,
            evs,
            roster,
            t.room,
            who,
            author_kind(roster, who),
            &body,
            t.message,
            false,
            &[],
        )?;
        update_event(tx, evs, id, who)
    })
}

/// 背景工作：釋放過期租約。
pub fn expire_all(core: &Core) -> Result<Vec<i64>> {
    write(core, expire)
}

// ---------------- 停滯清單 ----------------

/// 超過幾分鐘沒有任務更新算停住（AAF_STALL_MINUTES，預設 30；0＝關閉）。
fn stall_minutes(core: &Core) -> i64 {
    core.config.parse("AAF_STALL_MINUTES").unwrap_or(30).max(0)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Stalled {
    #[serde(flatten)]
    pub task: Task,
    pub idle_secs: i64,
    /// stalled｜waiting_external（blocked）
    pub kind: &'static str,
    pub wake_target: Option<String>,
    pub wake_note: Option<String>,
    pub last_woken_at: Option<i64>,
}

/// 叫醒對象：assignee 是本群非 manual 角色 → 它；否則本群 rank=lead 的角色（字母序第一）；都沒有 → None＋原因。
fn wake_target(
    tx: &Transaction,
    roster: &Roster,
    t: &Task,
) -> Result<(Option<String>, Option<String>)> {
    let ok = |n: &str| roster.roles.get(n).is_some_and(|r| !r.driver.is_manual());
    let roles: Vec<String> = store::members(tx, t.room)?
        .into_iter()
        .filter(|m| m.kind == "role")
        .map(|m| m.member)
        .collect();
    if let Some(a) = t.assignee.as_deref() {
        if roles.iter().any(|r| r == a) && ok(a) {
            return Ok((Some(a.to_string()), None));
        }
    }
    // 群有指定 lead 就只認它；沒指定才看全域 rank
    let designated = store::room_lead(tx, t.room)?;
    let mut leads: Vec<&String> = roles
        .iter()
        .filter(|r| {
            ok(r)
                && match designated.as_deref() {
                    Some(l) => l == r.as_str(),
                    None => roster.roles.get(*r).is_some_and(|s| s.rank == "lead"),
                }
        })
        .collect();
    leads.sort();
    if leads.is_empty() {
        if let Some(l) = designated.as_deref() {
            return Ok((
                None,
                Some(format!(
                    "本群 lead {l} 目前無法叫醒（不在群內或為手動角色）"
                )),
            ));
        }
    }
    match leads.first() {
        Some(l) => Ok((Some((*l).clone()), None)),
        None => Ok((
            None,
            Some(match t.assignee.as_deref() {
                Some(a) => format!("{a} 不是本群可叫醒的角色，本群也沒有帶隊"),
                None => "沒有負責人，本群也沒有帶隊".to_string(),
            }),
        )),
    }
}

fn last_woken(tx: &Transaction, id: i64, since: i64) -> Result<Option<i64>> {
    Ok(tx.query_row(
        "SELECT max(at) FROM chat_task_wakes WHERE task = ?1 AND at >= ?2",
        params![id, since],
        |r| r.get(0),
    )?)
}

fn stalled_tx(
    tx: &Transaction,
    mins: i64,
    roster: &Roster,
    room_id: i64,
    busy: &dyn Fn(&str) -> bool,
) -> Result<Vec<Stalled>> {
    if mins == 0 {
        return Ok(Vec::new());
    }
    let cutoff = now() - mins * 60;
    let tasks: Vec<Task> = {
        let mut st = tx.prepare(&format!(
            "SELECT {COLS} FROM chat_tasks WHERE room = ?1 AND state IN ('open','claimed','blocked') AND updated_at <= ?2 ORDER BY updated_at, id"
        ))?;
        let v = st
            .query_map(params![room_id, cutoff], row)?
            .collect::<rusqlite::Result<_>>()?;
        v
    };
    let mut out = Vec::new();
    for t in tasks {
        let (target, note) = wake_target(tx, roster, &t)?;
        if target.as_deref().is_some_and(busy) {
            continue; // 對象正在跑回合，不算停住
        }
        let last = last_woken(tx, t.id, t.updated_at)?;
        out.push(Stalled {
            idle_secs: now() - t.updated_at,
            kind: if t.state == "blocked" {
                "waiting_external"
            } else {
                "stalled"
            },
            wake_target: target,
            wake_note: note,
            last_woken_at: last,
            task: t,
        });
    }
    Ok(out)
}

/// 群內停住的任務（先跑 expire）。busy(role)＝該角色在本群的 lane 正在跑回合。
pub fn stalled(
    core: &Core,
    roster: &Roster,
    room_id: i64,
    busy: &dyn Fn(&str) -> bool,
) -> Result<Vec<Stalled>> {
    let mins = stall_minutes(core);
    write(core, |tx, evs| {
        expire(tx, evs)?;
        stalled_tx(tx, mins, roster, room_id, busy)
    })
}

/// 手動叫醒停住任務的負責人：貼一則系統訊息 @對象。不自動重試、不排程。
pub fn wake(
    core: &Core,
    roster: &Roster,
    room_id: i64,
    id: i64,
    by: &str,
    busy: &dyn Fn(&str) -> bool,
) -> Result<Stalled> {
    let mins = stall_minutes(core);
    if mins == 0 {
        return Err(Error::Other("停滯清單已關閉".into()));
    }
    write(core, |tx, evs| {
        expire(tx, evs)?;
        active_room(tx, room_id)?;
        let list = stalled_tx(tx, mins, roster, room_id, busy)?;
        let s = list
            .into_iter()
            .find(|s| s.task.id == id)
            .ok_or_else(|| Error::Other("task is not stalled".into()))?;
        let target = s.wake_target.clone().ok_or_else(|| {
            Error::Other(format!(
                "no one to wake: {}",
                s.wake_note.clone().unwrap_or_default()
            ))
        })?;
        if let Some(at) = s.last_woken_at {
            if now() - at < mins * 60 {
                let hm: String = tx.query_row(
                    "SELECT strftime('%H:%M', ?1, 'unixepoch', 'localtime')",
                    [at],
                    |r| r.get(0),
                )?;
                return Err(Error::Other(format!("已於 {hm} 叫醒")));
            }
        }
        let m = s.idle_secs / 60;
        let body = format!(
            "@{target} 任務 #{id}「{}」已 {m} 分鐘沒有進度。請接續，或用 aaf task done／blocked 回報。",
            // 標題裡的 @ 換成全形，避免標題提及的角色也被叫醒；只送 wake_target。
            s.task.title.replace('@', "＠")
        );
        let msg = post_system(tx, evs, roster, room_id, &body)?;
        tx.execute(
            "INSERT INTO chat_task_wakes(task, target, by, message, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, target, by, msg.id, now()],
        )?;
        evs.push(EventBus::record(
            tx,
            "chat.task.woken",
            1,
            json!({"task": s.task, "target": target, "by": by, "message": msg.id}),
        )?);
        Ok(Stalled {
            last_woken_at: Some(now()),
            ..s
        })
    })
}
