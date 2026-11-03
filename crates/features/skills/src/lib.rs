//! Skills 功能模組（源自先前的 Python 版）。
//!
//! - 只有人（Owner／Human）能改；agent（Lead／Worker）只能讀。agent 想改請在群裡提案。
//! - 指派的 skill 全文在該 lane 第一回合送給 agent，內容改了才再送；其他 skill 只給清單與路徑。

use aaf_api::error::{ApiError, ApiResult};
use aaf_api::{AppState, HttpFeature, User};
use aaf_core::Feature;
use axum::extract::{Path, Query};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub mod store;
pub use store::{Assign, Store};

pub struct Skills {
    pub store: Store,
}

impl Skills {
    pub fn new(instance: &std::path::Path) -> Self {
        Self {
            store: Store::new(instance),
        }
    }

    /// 給 dispatcher 的長內容：(指紋, 內容)。沒有任何 skill 時回 None。
    pub fn context_for(&self, role: &str, room: Option<i64>) -> Option<(u64, String)> {
        self.context_for_with(role, room, &[], &[])
    }

    /// 同上，另加額外的包（群級 lead 在該群帶 `lead` 包）。
    pub fn context_for_with(
        &self,
        role: &str,
        room: Option<i64>,
        extra_packs: &[&str],
        drop_packs: &[&str],
    ) -> Option<(u64, String)> {
        let text = self.store.context_with(role, room, extra_packs, drop_packs);
        if text.is_empty() {
            return None;
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        Some((h.finish(), text))
    }
}

type S = Extension<Arc<Skills>>;

fn err(e: store::SkillError) -> ApiError {
    match e.0 {
        400 | 413 => ApiError::BadRequest(e.1),
        404 => ApiError::NotFound,
        409 => ApiError::Conflict(e.1),
        _ => ApiError::Internal(e.1),
    }
}

fn human(u: &User) -> ApiResult<()> {
    if u.is_role() {
        return Err(ApiError::Forbidden(
            "skills can only be changed by people; post a proposal in the room instead".into(),
        ));
    }
    Ok(())
}

async fn list(Extension(sk): S, _u: User) -> Json<Value> {
    let c = sk.store.config();
    Json(
        json!({"skills": sk.store.listing(), "packs": c.packs, "roles": c.roles, "rooms": c.rooms, "dir": sk.store.dir()}),
    )
}

async fn read(Extension(sk): S, _u: User, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let text = sk.store.read(&id).map_err(err)?;
    Ok(Json(json!({"id": id, "content": text})))
}

#[derive(Deserialize)]
struct NewSkill {
    id: String,
    content: String,
}

async fn create(Extension(sk): S, u: User, Json(b): Json<NewSkill>) -> ApiResult<Json<Value>> {
    human(&u)?;
    Ok(Json(
        sk.store
            .save(&b.id, &b.content, &u.name, true, "")
            .map_err(err)?,
    ))
}

#[derive(Deserialize)]
struct Edit {
    content: String,
    #[serde(default)]
    note: String,
}

async fn edit(
    Extension(sk): S,
    u: User,
    Path(id): Path<String>,
    Json(b): Json<Edit>,
) -> ApiResult<Json<Value>> {
    human(&u)?;
    Ok(Json(
        sk.store
            .save(&id, &b.content, &u.name, false, &b.note)
            .map_err(err)?,
    ))
}

async fn delete(Extension(sk): S, u: User, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    human(&u)?;
    Ok(Json(sk.store.delete(&id, &u.name).map_err(err)?))
}

async fn history(Extension(sk): S, _u: User, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (v, log) = sk.store.history(&id).map_err(err)?;
    Ok(Json(json!({"versions": v, "log": log})))
}

async fn version(
    Extension(sk): S,
    _u: User,
    Path((id, ver)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"id": id, "version": ver, "content": sk.store.read_version(&id, &ver).map_err(err)?}),
    ))
}

#[derive(Deserialize)]
struct Restore {
    version: String,
}

async fn restore(
    Extension(sk): S,
    u: User,
    Path(id): Path<String>,
    Json(b): Json<Restore>,
) -> ApiResult<Json<Value>> {
    human(&u)?;
    Ok(Json(
        sk.store.restore(&id, &b.version, &u.name).map_err(err)?,
    ))
}

#[derive(Deserialize)]
struct Pack {
    skills: Vec<String>,
}

async fn set_pack(
    Extension(sk): S,
    u: User,
    Path(name): Path<String>,
    Json(b): Json<Pack>,
) -> ApiResult<Json<Value>> {
    human(&u)?;
    Ok(Json(
        json!({"pack": name, "skills": sk.store.set_pack(&name, &b.skills, &u.name).map_err(err)?}),
    ))
}

async fn delete_pack(
    Extension(sk): S,
    u: User,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    human(&u)?;
    sk.store.delete_pack(&name, &u.name).map_err(err)?;
    Ok(Json(json!({"ok": true})))
}

async fn assign(
    Extension(sk): S,
    u: User,
    Path((kind, key)): Path<(String, String)>,
    Json(b): Json<Assign>,
) -> ApiResult<Json<Value>> {
    human(&u)?;
    match kind.as_str() {
        "role" if aaf_runtime_name_ok(&key) => {}
        "room" if key.parse::<i64>().is_ok_and(|n| n > 0) => {}
        _ => {
            return Err(ApiError::BadRequest(
                "assign to role/<name> or room/<id>".into(),
            ))
        }
    }
    Ok(Json(json!(sk
        .store
        .assign(&kind, &key, &b, &u.name)
        .map_err(err)?)))
}

fn aaf_runtime_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Deserialize)]
struct PreviewQ {
    role: String,
    #[serde(default)]
    room: Option<i64>,
}

/// 預覽：角色（＋群）實際載入哪些 skill，以及送給 agent 的內容。
async fn preview(Extension(sk): S, _u: User, Query(q): Query<PreviewQ>) -> Json<Value> {
    Json(
        json!({"skills": sk.store.compose(&q.role, q.room), "context": sk.store.context(&q.role, q.room)}),
    )
}

impl Feature for Skills {
    fn name(&self) -> &'static str {
        "skills"
    }
}

impl HttpFeature for Skills {
    fn name(&self) -> &'static str {
        "skills"
    }
    fn routes(&self) -> Router<AppState> {
        let me = Arc::new(Skills {
            store: self.store.clone(),
        });
        Router::new()
            .route("/items", get(list).post(create))
            .route("/preview", get(preview))
            .route("/packs/{name}", put(set_pack).delete(delete_pack))
            .route("/assign/{kind}/{key}", put(assign))
            .route("/items/{id}", get(read).put(edit).delete(delete))
            .route("/items/{id}/history", get(history))
            .route("/items/{id}/history/{ver}", get(version))
            .route("/items/{id}/restore", post(restore))
            .layer(Extension(me))
    }
    fn nav(&self) -> Vec<(&'static str, &'static str)> {
        vec![("Skills", "/skills")]
    }
}
