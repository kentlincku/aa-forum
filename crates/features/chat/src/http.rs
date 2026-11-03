//! 群聊 HTTP 路由（掛在 /api/v1/chat/）。

use crate::store::{self, Room};
use crate::Shared;
use aaf_api::error::{ApiError, ApiResult};
use aaf_api::{AppState, User};
use aaf_core::perms::Rank;
use aaf_runtime::dispatcher::Lane;
use aaf_runtime::PermissionDecision;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

type S = Extension<Arc<Shared>>;

fn rank_name(u: &User) -> &'static str {
    match u.rank {
        Rank::Owner | Rank::Human => "human",
        Rank::Lead => "lead",
        Rank::Worker => "worker",
    }
}

fn bad(e: aaf_core::Error) -> ApiError {
    match e {
        aaf_core::Error::Other(m) => ApiError::BadRequest(m),
        aaf_core::Error::Denied(m) => ApiError::Forbidden(m),
        other => ApiError::Internal(other.to_string()),
    }
}

/// 可見性：成員可見；Owner 可見全部（含垃圾桶）。
fn visible_room(s: &AppState, u: &User, id: i64) -> ApiResult<Room> {
    let (r, member) = s
        .core
        .db
        .with(|c| Ok((store::room(c, id)?, store::is_member(c, id, &u.name)?)))?;
    match r {
        Some(r) if u.rank == Rank::Owner || (member && r.state != "deleted") => Ok(r),
        _ => Err(ApiError::NotFound),
    }
}

#[derive(Deserialize)]
struct ListQ {
    #[serde(default)]
    state: Option<String>,
}

async fn unread(State(s): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    let v: Vec<Value> = store::unread_counts(&s.core.db, &u.name)?
        .into_iter()
        .map(|(room, n, m)| json!({"room": room, "unread": n, "mentions": m}))
        .collect();
    Ok(Json(json!(v)))
}

#[derive(Deserialize)]
struct Read {
    through: i64,
}

async fn read(
    State(s): State<AppState>,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<Read>,
) -> ApiResult<Json<Value>> {
    visible_room(&s, &u, id)?;
    if !s.core.db.with(|c| store::is_member(c, id, &u.name))? {
        return Err(ApiError::Forbidden("not a member of this room".into()));
    }
    store::mark_read(&s.core.db, id, &u.name, b.through)?;
    Ok(Json(json!({"ok": true})))
}

async fn list(
    State(s): State<AppState>,
    u: User,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Vec<Room>>> {
    let state = q.state.unwrap_or_else(|| "active".into());
    if !["active", "archived", "deleted"].contains(&state.as_str()) {
        return Err(ApiError::BadRequest("state".into()));
    }
    if state == "deleted" {
        u.require(&s, "chat.room.archive")?;
    }
    Ok(Json(store::list_rooms(
        &s.core.db,
        &u.name,
        u.rank == Rank::Owner,
        &state,
    )?))
}

#[derive(Deserialize)]
struct CreateRoom {
    name: String,
    #[serde(default)]
    members: Vec<String>,
}

async fn create(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Json(b): Json<CreateRoom>,
) -> ApiResult<Json<Room>> {
    u.require(&s, "chat.room.create")?;
    let r = store::write(&s.core, |tx, evs| {
        store::create_room(tx, evs, &sh.roster(), &b.name, &u.name, &b.members)
    })
    .map_err(bad)?;
    Ok(Json(r))
}

async fn get_room(
    State(s): State<AppState>,
    u: User,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let r = visible_room(&s, &u, id)?;
    let (members, lead) = s
        .core
        .db
        .with(|c| Ok((store::members(c, id)?, store::room_lead(c, id)?)))?;
    Ok(Json(json!({"room": r, "members": members, "lead": lead})))
}

#[derive(Deserialize)]
struct MsgQ {
    #[serde(default)]
    after: i64,
    /// 以此訊息為中心載入（跳到搜尋結果）
    #[serde(default)]
    around: Option<i64>,
    /// 取最新的 limit 則（開群時）
    #[serde(default)]
    latest: bool,
    #[serde(default = "lim")]
    limit: i64,
}
fn lim() -> i64 {
    200
}

async fn list_messages(
    State(s): State<AppState>,
    u: User,
    Path(id): Path<i64>,
    Query(q): Query<MsgQ>,
) -> ApiResult<Json<Vec<store::Message>>> {
    visible_room(&s, &u, id)?;
    if let Some(c) = q.around {
        return Ok(Json(store::messages_around(&s.core.db, id, c, 100)?));
    }
    if q.latest {
        return Ok(Json(store::messages_latest(&s.core.db, id, q.limit)?));
    }
    Ok(Json(store::messages(&s.core.db, id, q.after, q.limit)?))
}

#[derive(Deserialize)]
struct Post {
    #[serde(default)]
    body: String,
    #[serde(default)]
    reply_to: Option<i64>,
    /// 先上傳取得的附件 id
    #[serde(default)]
    attachments: Vec<i64>,
}

async fn post_message(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<Post>,
) -> ApiResult<Json<Value>> {
    u.require(&s, "chat.message.post")?;
    let r = visible_room(&s, &u, id)?;
    if !s.core.db.with(|c| store::is_member(c, id, &u.name))? {
        return Err(ApiError::Forbidden("not a member of this room".into()));
    }
    let hook_ev = json!({"type": "chat.post", "sender": u.name, "rank": rank_name(&u), "room": id, "body": b.body, "reply_to": b.reply_to, "attachments": b.attachments.len()});
    if let Some(why) = sh.hook_pre(&hook_ev).await {
        return Err(ApiError::Unprocessable(format!("blocked by rule: {why}")));
    }
    let _ = r;
    let (m, to) = store::write(&s.core, |tx, evs| {
        store::post_message_opts(
            tx,
            evs,
            &sh.roster(),
            id,
            &u.name,
            if u.is_role() { "role" } else { "user" },
            &b.body,
            b.reply_to,
            true,
            &b.attachments,
        )
    })
    .map_err(bad)?;
    sh.hook_post(&json!({"type": "chat.post", "sender": u.name, "rank": rank_name(&u), "room": id, "body": b.body, "message_id": m.id})).await;
    Ok(Json(json!({"message": m, "delivered_to": to})))
}

#[derive(Deserialize)]
struct Invite {
    member: String,
}

async fn invite(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<Invite>,
) -> ApiResult<Json<Value>> {
    u.require(&s, "chat.room.manage")?;
    let r = visible_room(&s, &u, id)?;
    if r.state != "active" {
        return Err(ApiError::Conflict(format!("room is {}", r.state)));
    }
    let added = store::write(&s.core, |tx, evs| {
        let added = store::add_member_tx(tx, &sh.roster(), id, &b.member)?;
        if added {
            store::post_member_added(tx, evs, &sh.roster(), id, &u.name, &b.member)?;
        }
        if added {
            evs.push(aaf_core::events::EventBus::record(
                tx,
                "chat.member.added",
                1,
                json!({"room": id, "member": b.member, "by": u.name}),
            )?);
        }
        Ok(added)
    })
    .map_err(bad)?;
    Ok(Json(json!({"added": added})))
}

/// 移除成員（人或角色）。不能移除自己；角色這一輪還在跑就拒絕，請先按停止。
async fn remove_member(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path((id, member)): Path<(i64, String)>,
) -> ApiResult<Json<Value>> {
    u.require(&s, "chat.room.manage")?;
    let r = visible_room(&s, &u, id)?;
    if r.state != "active" {
        return Err(ApiError::Conflict(format!("room is {}", r.state)));
    }
    if member == u.name {
        return Err(ApiError::Conflict("cannot remove yourself".into()));
    }
    let lane = sh.roster().lane_for(&member, Some(id));
    let d = sh.dispatcher(&s.core);
    if d.busy(&lane) {
        return Err(ApiError::Conflict("turn in progress; stop it first".into()));
    }
    let removed = store::write(&s.core, |tx, evs| {
        store::remove_member_tx(tx, evs, &sh.roster(), id, &u.name, &member)
    })
    .map_err(bad)?;
    // 本群專屬的 lane 一併關掉（共用的預設 lane 還在服務其他群，不動）
    if removed && lane.room == Some(id) && d.lane_driver(&lane).await.is_some() {
        d.close(&lane, false).await?;
    }
    Ok(Json(json!({"removed": removed})))
}

#[derive(Deserialize)]
struct Manage {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
    #[serde(default)]
    frozen: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
}

async fn manage(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<Manage>,
) -> ApiResult<Json<Room>> {
    u.require(&s, "chat.room.manage")?;
    visible_room(&s, &u, id)?;
    if b.frozen.is_some() && b.reason.as_deref().is_none_or(|r| r.trim().is_empty()) {
        return Err(ApiError::BadRequest(
            "freezing or unfreezing requires a reason".into(),
        ));
    }
    let r = store::write(&s.core, |tx, evs| {
        if let Some(n) = &b.name {
            tx.execute("UPDATE chat_rooms SET name = ?1 WHERE id = ?2", rusqlite::params![store::validate_name(n)?, id])?;
        }
        if let Some(f) = &b.folder {
            tx.execute("UPDATE chat_rooms SET folder = ?1 WHERE id = ?2", rusqlite::params![f.trim(), id])?;
        }
        if let Some(p) = b.pinned {
            tx.execute("UPDATE chat_rooms SET pinned = ?1 WHERE id = ?2", rusqlite::params![p, id])?;
        }
        if let Some(fz) = b.frozen {
            let was = store::room(tx, id)?.is_some_and(|r| r.frozen);
            tx.execute("UPDATE chat_rooms SET frozen = ?1 WHERE id = ?2", rusqlite::params![fz, id])?;
            if was != fz {
                let reason = b.reason.clone().unwrap_or_default();
                let body = if fz {
                    format!("Room frozen by {}. Agents will not be woken until it is unfrozen.\nReason: {}", u.name, reason.trim())
                } else {
                    format!("Room unfrozen by {}. Agents will be woken again.\nReason: {}", u.name, reason.trim())
                };
                store::post_system(tx, evs, &sh.roster(), id, &body)?;
            }
            let r = store::room(tx, id)?.expect("exists");
            tx.execute(
                "INSERT INTO chat_room_audit(room, room_name, action, actor, detail, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![id, r.name, if fz { "frozen" } else { "unfrozen" }, u.name, b.reason.clone().unwrap_or_default(), store::now()],
            )?;
        }
        let r = store::room(tx, id)?.expect("exists");
        evs.push(aaf_core::events::EventBus::record(tx, "chat.room.updated", 1, json!({"room": r, "by": u.name, "reason": b.reason}))?);
        Ok(r)
    })
    .map_err(bad)?;
    Ok(Json(r))
}

#[derive(Deserialize)]
struct StateChange {
    action: String,
}

async fn change_state(
    State(s): State<AppState>,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<StateChange>,
) -> ApiResult<Json<Room>> {
    u.require(&s, "chat.room.archive")?;
    visible_room(&s, &u, id)?;
    if !["archive", "delete", "restore"].contains(&b.action.as_str()) {
        return Err(ApiError::BadRequest(
            "action must be archive, delete or restore".into(),
        ));
    }
    Ok(Json(
        store::set_state(&s.core, id, &b.action, &u.name).map_err(|e| match e {
            aaf_core::Error::Other(m) => ApiError::Conflict(m),
            o => bad(o),
        })?,
    ))
}

#[derive(Deserialize)]
struct Purge {
    /// 須輸入群名確認
    confirm_name: String,
    /// 一併刪除已匯出的逐字稿（預設保留）
    #[serde(default)]
    delete_transcripts: bool,
}

async fn purge(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<Purge>,
) -> ApiResult<Json<Value>> {
    u.require(&s, "chat.room.purge")?;
    let r = visible_room(&s, &u, id)?;
    if b.confirm_name != r.name {
        return Err(ApiError::BadRequest(
            "confirm_name does not match the room name".into(),
        ));
    }
    let d = sh.dispatcher(&s.core);
    store::purge(&s.core, &d, id, &u.name)
        .await
        .map_err(|e| match e {
            aaf_core::Error::Other(m) => ApiError::Conflict(m),
            o => bad(o),
        })?;
    let removed = if b.delete_transcripts {
        crate::transcript::remove_room(&sh.transcript_root(&s.core), id).map_err(bad)?
    } else {
        false
    };
    Ok(Json(json!({"purged": id, "transcripts_deleted": removed})))
}

async fn audit(
    State(s): State<AppState>,
    u: User,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<Value>>> {
    if u.rank != Rank::Owner {
        visible_room(&s, &u, id)?;
    }
    Ok(Json(store::audit_log(&s.core.db, id)?))
}

async fn lanes(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    visible_room(&s, &u, id)?;
    let d = sh.dispatcher(&s.core);
    let mut v = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (l, st) in d
        .lane_states()
        .await
        .into_iter()
        .filter(|(l, _)| l.room == Some(id))
    {
        let pending = match d.lane_driver(&l).await {
            Some(dr) => dr.pending_permissions(),
            None => vec![],
        };
        let stuck = d.blocked(&l).await;
        seen.insert(l.role.clone());
        v.push(
            json!({"role": l.role, "status": st, "pending_permissions": pending, "stuck": stuck}),
        );
    }
    // 重啟後被擋住的 lane 還沒有 driver，不在 lane_states 裡：逐一補查本群角色
    let roles: Vec<String> = s.core.db.with(|c| {
        Ok(store::members(c, id)?
            .into_iter()
            .filter(|m| m.kind == "role")
            .map(|m| m.member)
            .collect())
    })?;
    for role in roles.into_iter().filter(|r| !seen.contains(r)) {
        let l = sh.roster().lane_for(&role, Some(id));
        if let Some(stuck) = d.blocked(&l).await {
            v.push(json!({"role": role, "status": "offline", "pending_permissions": [], "stuck": stuck}));
        }
    }
    Ok(Json(json!({"lanes": v})))
}

async fn cancel(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path((id, role)): Path<(i64, String)>,
) -> ApiResult<Json<Value>> {
    u.require(&s, "chat.agent.control")?;
    visible_room(&s, &u, id)?;
    let lane = sh.roster().lane_for(&role, Some(id));
    let found = sh.dispatcher(&s.core).cancel(&lane).await?;
    Ok(Json(json!({"cancelled": found})))
}

#[derive(Deserialize)]
struct CloseQ {
    #[serde(default)]
    forget: bool,
}

async fn close(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path((id, role)): Path<(i64, String)>,
    Query(q): Query<CloseQ>,
) -> ApiResult<Json<Value>> {
    // Close is also the explicit durable-context recovery authorization.
    if u.rank != Rank::Owner {
        return Err(ApiError::Forbidden(
            "only owner can explicitly recover a lane".into(),
        ));
    }
    u.require(&s, "chat.agent.control")?;
    visible_room(&s, &u, id)?;
    let lane = sh.roster().lane_for(&role, Some(id));
    let d = sh.dispatcher(&s.core);
    if d.busy(&lane) {
        return Err(ApiError::Conflict("turn in progress; stop it first".into()));
    }
    d.close(&lane, q.forget).await?;
    Ok(Json(json!({"closed": true})))
}

#[derive(Deserialize)]
struct PermAnswer {
    request_id: u64,
    /// 選項 id；省略＝取消
    #[serde(default)]
    option_id: Option<String>,
}

async fn answer_permission(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path((id, role)): Path<(i64, String)>,
    Json(b): Json<PermAnswer>,
) -> ApiResult<Json<Value>> {
    u.require(&s, "chat.agent.control")?;
    visible_room(&s, &u, id)?;
    let lane: Lane = sh.roster().lane_for(&role, Some(id));
    let d = sh
        .dispatcher(&s.core)
        .lane_driver(&lane)
        .await
        .ok_or(ApiError::NotFound)?;
    let decision = match b.option_id {
        Some(o) => PermissionDecision::Selected { option_id: o },
        None => PermissionDecision::Cancelled,
    };
    d.respond_permission(b.request_id, decision)
        .await
        .map_err(|e| ApiError::Conflict(e.to_string()))?;
    Ok(Json(json!({"ok": true})))
}

async fn roles(State(s): State<AppState>, Extension(sh): S, _u: User) -> Json<Value> {
    let draining = sh.dispatcher(&s.core).draining();
    let mut v: Vec<Value> = sh.roster()
        .roles
        .iter()
        .map(
            |(n, r)| json!({"name": n, "manual": r.driver.is_manual(), "room_lanes": r.room_lanes, "draining": draining.contains(n)}),
        )
        .collect();
    v.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(json!(v))
}

/// 立即重讀 roles.json（Owner）。回傳差異與仍在收尾的角色。
async fn reload_roles(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
) -> ApiResult<Json<Value>> {
    if u.rank != Rank::Owner {
        return Err(ApiError::Forbidden("owner only".into()));
    }
    let diff = sh
        .reload(&s.core)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(
        json!({"diff": diff, "draining": sh.dispatcher(&s.core).draining()}),
    ))
}

// ---------------- 任務 ----------------

fn task_err(e: aaf_core::Error) -> ApiError {
    match e {
        aaf_core::Error::Denied(m) => ApiError::Forbidden(m),
        aaf_core::Error::Other(m) if m.contains("not found") => ApiError::NotFound,
        aaf_core::Error::Other(m) => ApiError::Conflict(m),
        o => ApiError::Internal(o.to_string()),
    }
}

/// 任務所在群：必須看得到（成員；Owner 全看）。
fn task_room(s: &AppState, u: &User, id: i64) -> ApiResult<crate::tasks::Task> {
    let t = s
        .core
        .db
        .with(|c| crate::tasks::get(c, id))?
        .ok_or(ApiError::NotFound)?;
    visible_room(s, u, t.room)?;
    Ok(t)
}

#[derive(Deserialize)]
struct TaskQ {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    mine: bool,
}

async fn room_tasks(
    State(s): State<AppState>,
    u: User,
    Path(id): Path<i64>,
    Query(q): Query<TaskQ>,
) -> ApiResult<Json<Vec<crate::tasks::Task>>> {
    visible_room(&s, &u, id)?;
    Ok(Json(crate::tasks::list(&s.core, id, q.state.as_deref())?))
}

/// 群內某角色的 lane 是否正在跑回合。
fn busy_fn(sh: &Arc<Shared>, s: &AppState, room: i64) -> impl Fn(&str) -> bool {
    let roster = sh.roster();
    let d = sh.dispatcher(&s.core);
    move |role: &str| d.busy(&roster.lane_for(role, Some(room)))
}

/// 群成員（Owner 也得是成員才看得到這份清單；非成員 403）。
fn member_room(s: &AppState, u: &User, id: i64) -> ApiResult<()> {
    let (r, member) = s
        .core
        .db
        .with(|c| Ok((store::room(c, id)?, store::is_member(c, id, &u.name)?)))?;
    match r {
        None => Err(ApiError::NotFound),
        Some(_) if !member => Err(ApiError::Forbidden("not a member of this room".into())),
        Some(_) => Ok(()),
    }
}

async fn stalled_tasks(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<crate::tasks::Stalled>>> {
    member_room(&s, &u, id)?;
    let busy = busy_fn(&sh, &s, id);
    Ok(Json(
        crate::tasks::stalled(&s.core, &sh.roster(), id, &busy).map_err(task_err)?,
    ))
}

async fn wake_task(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path((id, tid)): Path<(i64, i64)>,
) -> ApiResult<Json<crate::tasks::Stalled>> {
    member_room(&s, &u, id)?;
    let t = task_room(&s, &u, tid)?;
    if t.room != id {
        return Err(ApiError::NotFound);
    }
    let busy = busy_fn(&sh, &s, id);
    Ok(Json(
        crate::tasks::wake(&s.core, &sh.roster(), id, tid, &u.name, &busy).map_err(task_err)?,
    ))
}

#[derive(Deserialize)]
struct LeadBody {
    /// 角色 id；null／省略＝撤換
    #[serde(default)]
    role: Option<String>,
}

/// owner 指定／撤換本群 lead。
async fn set_lead(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<LeadBody>,
) -> ApiResult<Json<Value>> {
    if u.rank != Rank::Owner {
        return Err(ApiError::Forbidden("owner only".into()));
    }
    visible_room(&s, &u, id)?;
    let role = b.role.as_deref().map(str::trim).filter(|r| !r.is_empty());
    let lead = store::write(&s.core, |tx, evs| {
        store::set_room_lead(tx, evs, &sh.roster(), id, role, &u.name)
    })
    .map_err(task_err)?;
    Ok(Json(json!({"lead": lead})))
}

async fn my_tasks(
    State(s): State<AppState>,
    u: User,
    Query(q): Query<TaskQ>,
) -> ApiResult<Json<Vec<crate::tasks::Task>>> {
    Ok(Json(crate::tasks::list_for(
        &s.core,
        &u.name,
        q.mine,
        q.state.as_deref(),
    )?))
}

#[derive(Deserialize)]
struct NewTask {
    title: String,
    #[serde(default)]
    spec: String,
    #[serde(default)]
    assignee: Option<String>,
}

async fn create_task(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<NewTask>,
) -> ApiResult<Json<crate::tasks::Task>> {
    visible_room(&s, &u, id)?;
    if !s.core.db.with(|c| store::is_member(c, id, &u.name))? {
        return Err(ApiError::Forbidden("not a member of this room".into()));
    }
    // 角色派工依「這個群」判定 lead（群有指定以群為準，沒指定退回全域 rank）
    let rank = if u.is_role() {
        let roster = sh.roster();
        let lead = s
            .core
            .db
            .with(|c| store::is_lead_in(c, &roster, id, &u.name))?;
        if lead {
            Rank::Lead
        } else {
            Rank::Worker
        }
    } else {
        u.rank
    };
    s.core.policy.check(rank, "chat.task.post")?;
    let rank_s = match rank {
        Rank::Lead => "lead",
        Rank::Worker => "worker",
        _ => rank_name(&u),
    };
    let ev = json!({"type": "task.post", "sender": u.name, "rank": rank_s, "room": id, "body": format!("{}\n\n{}", b.title, b.spec), "to": b.assignee});
    if let Some(why) = sh.hook_pre(&ev).await {
        return Err(ApiError::Unprocessable(format!("blocked by rule: {why}")));
    }
    let t = crate::tasks::create(
        &s.core,
        &sh.roster(),
        id,
        &u.name,
        &b.title,
        &b.spec,
        b.assignee.as_deref(),
    )
    .map_err(task_err)?;
    sh.hook_post(&ev).await;
    Ok(Json(t))
}

async fn claim_task(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
) -> ApiResult<Json<crate::tasks::Task>> {
    u.require(&s, "chat.task.work")?;
    task_room(&s, &u, id)?;
    Ok(Json(
        crate::tasks::claim(
            &s.core,
            &sh.roster(),
            id,
            &u.name,
            crate::tasks::DEFAULT_LEASE_SECS,
        )
        .map_err(task_err)?,
    ))
}

#[derive(Deserialize)]
struct Report {
    state: String,
    #[serde(default)]
    result: String,
}

async fn report_task(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<Report>,
) -> ApiResult<Json<crate::tasks::Task>> {
    u.require(&s, "chat.task.work")?;
    task_room(&s, &u, id)?;
    Ok(Json(
        crate::tasks::report(&s.core, &sh.roster(), id, &u.name, &b.state, &b.result)
            .map_err(task_err)?,
    ))
}

#[derive(Deserialize)]
struct WhoamiQ {
    #[serde(default)]
    room: Option<i64>,
}

/// 身分。角色帶 ?room= 時回「在這個群」的 rank（群級 lead），另附全域 rank；
/// 不帶 room 或是人時維持原格式 {name, rank}。
async fn whoami(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Query(q): Query<WhoamiQ>,
) -> ApiResult<Json<Value>> {
    let base = json!({"name": u.name, "rank": rank_name_full(&u)});
    let (Some(room), true) = (q.room, u.is_role()) else {
        return Ok(Json(base));
    };
    let roster = sh.roster();
    let lead = s
        .core
        .db
        .with(|c| store::is_lead_in(c, &roster, room, &u.name))?;
    Ok(Json(json!({
        "name": u.name,
        "rank": if lead { "lead" } else { "worker" },
        "room": room,
        "global_rank": rank_name_full(&u),
    })))
}

fn rank_name_full(u: &User) -> &'static str {
    match u.rank {
        Rank::Owner => "owner",
        Rank::Human => "human",
        Rank::Lead => "lead",
        Rank::Worker => "worker",
    }
}

#[derive(Deserialize)]
struct ExportReq {
    #[serde(default)]
    rooms: Option<Vec<i64>>,
    #[serde(default)]
    since: Option<String>,
}

/// 立即匯出逐字稿（Owner）。
/// 頁面可改的群聊設定。agent_chain：同群自上一則人類發言起，角色連發超過幾則就不再互相叫醒（0＝不限制）。
async fn get_settings(State(s): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    if u.rank != Rank::Owner {
        return Err(ApiError::Forbidden("owner only".into()));
    }
    let (v, page) = s.core.db.with(|c| store::apply_agent_chain(c))?;
    Ok(Json(json!({
        "agent_chain": v,
        "agent_chain_source": if page { "page" } else { "config" },
        "agent_chain_config": store::AGENT_CHAIN_CONFIG.load(std::sync::atomic::Ordering::Relaxed),
        "agent_chain_max": store::AGENT_CHAIN_MAX,
    })))
}

#[derive(Deserialize)]
struct SettingsReq {
    /// 數字＝頁面自訂值；null＝清掉、回到設定檔的值
    agent_chain: Option<usize>,
}

async fn put_settings(
    State(s): State<AppState>,
    u: User,
    Json(b): Json<SettingsReq>,
) -> ApiResult<Json<Value>> {
    if u.rank != Rank::Owner {
        return Err(ApiError::Forbidden("owner only".into()));
    }
    if b.agent_chain.is_some_and(|n| n > store::AGENT_CHAIN_MAX) {
        return Err(ApiError::BadRequest(format!(
            "agent_chain must be 0..={} (0 = no limit)",
            store::AGENT_CHAIN_MAX
        )));
    }
    let v = b.agent_chain.map(|n| n.to_string());
    s.core
        .db
        .with(|c| store::set_setting(c, "agent_chain", v.as_deref(), &u.name))?;
    get_settings(State(s), u).await
}

async fn export_transcripts(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Json(b): Json<ExportReq>,
) -> ApiResult<Json<Value>> {
    if u.rank != Rank::Owner {
        return Err(ApiError::Forbidden("owner only".into()));
    }
    if let Some(d) = &b.since {
        if jiff::civil::Date::strptime("%Y-%m-%d", d).is_err() {
            return Err(ApiError::BadRequest("since must be YYYY-MM-DD".into()));
        }
    }
    let core = s.core.clone();
    let rep = tokio::task::spawn_blocking(move || {
        sh.export_transcripts(&core, b.rooms.as_deref(), b.since.as_deref())
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(bad)?;
    Ok(Json(json!(rep)))
}

// ---------------- 附件 ----------------

#[derive(Deserialize)]
struct UploadQ {
    name: String,
    /// image：當圖片處理（重新編碼、可預覽）；預設依副檔名判斷
    #[serde(default)]
    kind: Option<String>,
}

/// 上傳附件（原始位元組串流，非 multipart）：`POST /rooms/{id}/attachments?name=a.pdf`。
/// 需為群成員且群為 active；回傳附件 id，發訊息時放進 `attachments`。
async fn upload(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Query(q): Query<UploadQ>,
    req: axum::extract::Request,
) -> ApiResult<Json<crate::attachments::Attachment>> {
    u.require(&s, "chat.message.post")?;
    let r = visible_room(&s, &u, id)?;
    if r.state != "active" {
        return Err(ApiError::Conflict(format!("room is {}", r.state)));
    }
    if !s.core.db.with(|c| store::is_member(c, id, &u.name))? {
        return Err(ApiError::Forbidden("not a member of this room".into()));
    }
    let name = crate::attachments::clean_name(&q.name)
        .ok_or_else(|| ApiError::BadRequest("invalid file name".into()))?;
    let lower = name.to_ascii_lowercase();
    let as_image = match q.kind.as_deref() {
        Some("image") => true,
        Some("file") | None if q.kind.is_some() => false,
        _ => [".png", ".jpg", ".jpeg", ".webp"]
            .iter()
            .any(|e| lower.ends_with(e)),
    };
    let limit = if as_image {
        Some(crate::attachments::MAX_IMAGE_BYTES)
    } else {
        crate::attachments::file_limit(&s.core)
    };
    if let Some(len) = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if limit.is_some_and(|l| len > l) {
            return Err(ApiError::PayloadTooLarge(format!(
                "limit is {} MB",
                limit.unwrap() / 1048576
            )));
        }
    }
    let instance = sh.roster().instance.clone();
    let stream = req.into_body().into_data_stream();
    let (tmp, size, sha) = crate::attachments::spool(&instance, stream, limit)
        .await
        .map_err(|e| match e {
            aaf_core::Error::Other(m) if m.starts_with("too large") => ApiError::PayloadTooLarge(m),
            o => bad(o),
        })?;
    let core = s.core.clone();
    let who = u.name.clone();
    let a = tokio::task::spawn_blocking(move || {
        crate::attachments::create(&core, &instance, id, &who, &name, tmp, size, sha, as_image)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(bad)?;
    Ok(Json(a))
}

/// 下載／預覽附件。只給群成員（Owner 例外）；檔案一律下載，圖片可內嵌（只有我們重新編碼過的 PNG）。
async fn download(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path((id, aid)): Path<(i64, i64)>,
) -> ApiResult<axum::response::Response> {
    visible_room(&s, &u, id)?;
    let a = s
        .core
        .db
        .with(|c| crate::attachments::get(c, id, aid))?
        .ok_or(ApiError::NotFound)?;
    if a.message.is_none() && a.uploader != u.name {
        return Err(ApiError::NotFound);
    }
    let path = crate::attachments::blob_path(&sh.roster().instance, &a.sha256).map_err(bad)?;
    let f = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(f));
    let enc: String = a
        .name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let disp = if a.kind == "image" {
        "inline"
    } else {
        "attachment"
    };
    let mut res = axum::response::Response::new(body);
    let hd = res.headers_mut();
    use axum::http::{header, HeaderValue};
    hd.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if a.kind == "image" {
            "image/png"
        } else {
            "application/octet-stream"
        }),
    );
    hd.insert(header::CONTENT_LENGTH, HeaderValue::from(a.size as u64));
    hd.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "{disp}; filename=download; filename*=UTF-8''{enc}"
        ))
        .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    hd.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    hd.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    hd.insert(
        "content-security-policy",
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    Ok(res)
}

// ---------------- 自動核准、資料夾 ----------------

fn tz(s: &AppState) -> Option<String> {
    s.core.config.get("AAF_TZ").ok().map(str::to_string)
}

async fn get_approval(
    State(s): State<AppState>,
    u: User,
    Path(id): Path<i64>,
) -> ApiResult<Json<crate::approval::Approval>> {
    visible_room(&s, &u, id)?;
    let today = crate::approval::today(tz(&s).as_deref());
    Ok(Json(
        s.core.db.with(|c| crate::approval::state(c, id, &today))?,
    ))
}

#[derive(Deserialize)]
struct ApprovalReq {
    on: bool,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    reason: String,
}

async fn set_approval(
    State(s): State<AppState>,
    Extension(sh): S,
    u: User,
    Path(id): Path<i64>,
    Json(b): Json<ApprovalReq>,
) -> ApiResult<Json<Value>> {
    if u.is_role() {
        return Err(ApiError::Forbidden(
            "auto-approve can only be changed by people".into(),
        ));
    }
    visible_room(&s, &u, id)?;
    if !s.core.db.with(|c| store::is_member(c, id, &u.name))? && u.rank != Rank::Owner {
        return Err(ApiError::Forbidden("not a member of this room".into()));
    }
    let today = crate::approval::today(tz(&s).as_deref());
    let until = b.until.as_deref().filter(|x| !x.trim().is_empty());
    let (st, changed) = crate::approval::set(
        &s.core,
        &sh.roster(),
        id,
        &u.name,
        b.on,
        until,
        &b.reason,
        &today,
    )
    .map_err(|e| match e {
        aaf_core::Error::Other(m) if m.contains("until") => ApiError::BadRequest(m),
        o => bad(o),
    })?;
    Ok(Json(json!({"approval": st, "changed": changed})))
}

async fn folders(State(s): State<AppState>, _u: User) -> ApiResult<Json<Value>> {
    let f: Vec<(String, i64)> = s.core.db.with(|c| store::folders(c))?;
    Ok(Json(json!(f
        .into_iter()
        .map(|(name, rooms)| json!({"name": name, "rooms": rooms}))
        .collect::<Vec<_>>())))
}

// ---------------- 搜尋 ----------------

#[derive(Deserialize)]
struct SearchQ {
    #[serde(default)]
    q: String,
    #[serde(default)]
    room: Option<i64>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    mentions: bool,
    #[serde(default)]
    has_file: bool,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    until: Option<i64>,
    #[serde(default)]
    before: Option<i64>,
    #[serde(default = "search_lim")]
    limit: i64,
}
fn search_lim() -> i64 {
    50
}

/// 搜尋訊息：只含查詢者是成員的群（Owner 全部）；至少要有關鍵字或一個篩選條件。
async fn search(
    State(s): State<AppState>,
    u: User,
    Query(q): Query<SearchQ>,
) -> ApiResult<Json<Value>> {
    let text = q.q.trim();
    if text.chars().count() > 200 {
        return Err(ApiError::BadRequest("query too long".into()));
    }
    let filtered =
        q.from.is_some() || q.mentions || q.has_file || q.since.is_some() || q.until.is_some();
    if text.is_empty() && !filtered {
        return Err(ApiError::BadRequest(
            "type something to search, or pick a filter".into(),
        ));
    }
    if let Some(r) = q.room {
        visible_room(&s, &u, r)?;
    }
    let query = crate::search::Query {
        text,
        viewer: &u.name,
        all_rooms: u.rank == Rank::Owner,
        room: q.room,
        from: q.from.as_deref().filter(|f| !f.is_empty()),
        mentions_me: q.mentions,
        has_attachment: q.has_file,
        since: q.since,
        until: q.until,
        before_id: q.before,
        limit: q.limit,
    };
    let hits = s.core.db.with(|c| crate::search::search(c, &query))?;
    let more = hits.len() as i64 >= q.limit.clamp(1, 100);
    Ok(Json(
        json!({"hits": hits, "more": more, "terms": crate::search::terms(text)}),
    ))
}

pub fn routes(shared: Arc<Shared>) -> Router<AppState> {
    Router::new()
        .route("/roles", get(roles))
        .route("/roles/reload", post(reload_roles))
        .route("/whoami", get(whoami))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/folders", get(folders))
        .route("/search", get(search))
        .route("/rooms/{id}/approval", get(get_approval).post(set_approval))
        .route(
            "/rooms/{id}/attachments",
            post(upload).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/rooms/{id}/attachments/{aid}", get(download))
        .route("/transcripts/export", post(export_transcripts))
        .route("/tasks", get(my_tasks))
        .route("/tasks/{id}/claim", post(claim_task))
        .route("/tasks/{id}/report", post(report_task))
        .route("/rooms/{id}/tasks", get(room_tasks).post(create_task))
        .route("/rooms/{id}/stalled", get(stalled_tasks))
        .route("/rooms/{id}/lead", axum::routing::put(set_lead))
        .route("/rooms/{id}/tasks/{tid}/wake", post(wake_task))
        .route("/unread", get(unread))
        .route("/rooms/{id}/read", post(read))
        .route("/rooms", get(list).post(create))
        .route("/rooms/{id}", get(get_room).patch(manage))
        .route("/rooms/{id}/state", post(change_state))
        .route("/rooms/{id}/purge", post(purge))
        .route("/rooms/{id}/audit", get(audit))
        .route("/rooms/{id}/members", post(invite))
        .route("/rooms/{id}/members/{member}", delete(remove_member))
        .route(
            "/rooms/{id}/messages",
            get(list_messages).post(post_message),
        )
        .route("/rooms/{id}/lanes", get(lanes))
        .route("/rooms/{id}/lanes/{role}/cancel", post(cancel))
        .route("/rooms/{id}/lanes/{role}/close", post(close))
        .route(
            "/rooms/{id}/lanes/{role}/permission",
            post(answer_permission),
        )
        .layer(Extension(shared))
}
