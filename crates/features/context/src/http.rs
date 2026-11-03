//! Authenticated, room-scoped retrieval. Authorization is supplied by the host feature.
use aaf_api::{
    error::{ApiError, ApiResult},
    AppState, HttpFeature, User,
};
use aaf_core::{Core, Result};
use aaf_runtime::dispatcher::Lane;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Extension, Json, Router,
};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub type Access = Arc<dyn Fn(&Core, &User, &Lane) -> Result<bool> + Send + Sync>;
pub struct ContextHttp {
    access: Access,
}
impl ContextHttp {
    pub fn new(access: Access) -> Self {
        Self { access }
    }
}
impl HttpFeature for ContextHttp {
    fn name(&self) -> &'static str {
        "context"
    }
    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/rooms/{room}/roles/{role}/sources", get(read))
            .layer(Extension(self.access.clone()))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadQuery {
    #[serde(default)]
    after: i64,
    #[serde(default = "limit")]
    limit: usize,
    source: Option<i64>,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    raw: bool,
}
fn limit() -> usize {
    20
}
async fn read(
    State(s): State<AppState>,
    u: User,
    Extension(access): Extension<Access>,
    Path((room, role)): Path<(i64, String)>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<Value>> {
    u.require(&s, "context.source.read")?;
    if room <= 0 || !aaf_runtime::dispatcher::valid_role_name(&role) {
        return Err(ApiError::BadRequest("invalid lane".into()));
    }
    let lane = Lane::new(&role, Some(room));
    if (u.rank != aaf_core::perms::Rank::Owner && (!u.is_role() || u.name != role))
        || !access(&s.core, &u, &lane)?
    {
        return Err(ApiError::Forbidden(
            "context is restricted to your own member lane".into(),
        ));
    }
    if q.after < 0
        || q.limit == 0
        || q.limit > 100
        || q.source.is_some_and(|id| id <= 0)
        || (q.source.is_none() && q.offset != 0)
    {
        return Err(ApiError::BadRequest(
            "invalid context cursor or limit".into(),
        ));
    }
    let source_table = if q.raw {
        "context_sources"
    } else {
        "context_logical_sources"
    };
    if let Some(id) = q.source {
        let page_bytes = crate::config::Settings::parse(&s.core.config)?.page_bytes as i64;
        let row = s.core.db.with(|c| {
            Ok(c.query_row(&format!("SELECT length(CAST(body AS BLOB)),substr(CAST(body AS BLOB),?4,?5) FROM {source_table} WHERE role=?1 AND room=?2 AND id=?3"), params![role,room,id,q.offset.saturating_add(1) as i64,page_bytes], |r| Ok((r.get::<_,i64>(0)? as usize,r.get::<_,Vec<u8>>(1)?))).optional()?)
        })?.ok_or(ApiError::NotFound)?;
        let (total, mut bytes) = row;
        if q.offset > total {
            return Err(ApiError::BadRequest("offset beyond source".into()));
        }
        // Trim only an incomplete final code point. A non-boundary start is rejected.
        let text = loop {
            match String::from_utf8(bytes) {
                Ok(s) => break s,
                Err(e) if e.utf8_error().error_len().is_none() => {
                    let valid = e.utf8_error().valid_up_to();
                    bytes = e.into_bytes();
                    bytes.truncate(valid);
                }
                Err(_) => {
                    return Err(ApiError::BadRequest(
                        "offset is not a UTF-8 boundary".into(),
                    ))
                }
            }
        };
        let end = q.offset + text.len();
        return Ok(Json(
            json!({"source":id,"offset":q.offset,"text":text,"total_bytes":total,"next_offset":(end < total).then_some(end),"untrusted":true}),
        ));
    }
    let value = s.core.db.with(|c| {
        let mut st = c.prepare(&format!("SELECT id,kind,substr(body,1,256),length(CAST(body AS BLOB)) FROM {source_table} WHERE role=?1 AND room=?2 AND id>?3 ORDER BY id LIMIT ?4"))?;
        let rows = st.query_map(params![role,room,q.after,q.limit as i64], |r| Ok(json!({"id":r.get::<_,i64>(0)?,"kind":r.get::<_,String>(1)?,"excerpt":r.get::<_,String>(2)?,"bytes":r.get::<_,i64>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let last = rows.last().and_then(|r| r["id"].as_i64()).unwrap_or(q.after);
        let has_more: bool = c.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {source_table} WHERE role=?1 AND room=?2 AND id>?3)"),params![role,room,last],|r|r.get(0))?;
        let mut st = c.prepare("SELECT version,cursor,status FROM context_checkpoints WHERE role=?1 AND room=?2 ORDER BY version DESC LIMIT 20")?;
        let checkpoints = st.query_map(params![role,room],|r|Ok(json!({"version":r.get::<_,i64>(0)?,"cursor":r.get::<_,i64>(1)?,"status":r.get::<_,String>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let (pending,fault): (bool,Option<String>) = c.query_row("SELECT pending,fault FROM context_sessions WHERE role=?1 AND room=?2",params![role,room],|r| Ok((r.get(0)?,r.get(1)?))).optional()?.unwrap_or((false,None));
        let inflight: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM context_attempts WHERE role=?1 AND room=?2 AND state='inflight')",params![role,room],|r|r.get(0))?;
        let diagnostic=json!({"blocked":fault.is_some() || inflight,"fault":fault,"inflight":inflight,"pending":pending,"recovery":format!("Owner only: read back unknown effects, repair storage, wait/stop busy turn, then POST /api/v1/chat/rooms/{room}/lanes/{role}/close?forget=false. This explicitly authorizes queued retry, not external replay.")});
        Ok(json!({"sources":rows,"next_after":has_more.then_some(last),"checkpoints":checkpoints,"diagnostic":diagnostic,"raw":q.raw,"untrusted":true,"coverage":"Only AAF-observed sources. Unknown effects require readback, not tool replay."}))
    })?;
    Ok(Json(value))
}
