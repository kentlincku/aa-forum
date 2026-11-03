//! HTTP 介面層：`/api/v1`、登入與 session、SSE 事件串流、功能模組路由註冊、內嵌前端。
//!
//! 依賴方向：features 實作 [`HttpFeature`]（本 crate）掛路由；本 crate 不依賴任何 feature，由 apps/aaf 組裝。

pub mod auth;
pub mod error;
pub mod events;
pub mod static_files;

use aaf_core::Core;
use axum::Router;
use std::sync::Arc;

pub use auth::{issue_role_token, revoke_role_token, AuthFeature, User, ROLE_TOKEN_PREFIX};
pub use error::ApiError;

#[derive(Clone)]
pub struct AppState {
    pub core: Arc<Core>,
    pub features: Arc<Vec<Arc<dyn HttpFeature>>>,
}

/// 功能模組的 HTTP 擴充點：路由一律掛在 `/api/v1/<feature 名稱>/` 之下。
pub trait HttpFeature: Send + Sync {
    fn name(&self) -> &'static str;
    fn routes(&self) -> Router<AppState>;
    /// 前端導覽項目（顯示名稱, 路徑）；前端依此產生側欄，不改 Shell。
    fn nav(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
    /// SSE 可見性：此使用者能否看到這個事件。每個 feature 只判斷自己關心的 kind，其餘回 true；
    /// 全部 feature 都同意才送出（例如 chat 依群成員資格過濾 chat.* 與 runtime.*）。
    fn can_see(&self, _s: &AppState, _u: &User, _ev: &aaf_contract::EventEnvelope) -> bool {
        true
    }
}

/// 組裝整個 HTTP 應用。
pub fn app(core: Arc<Core>, features: &[Arc<dyn HttpFeature>]) -> Router {
    let nav: Vec<serde_json::Value> = features
        .iter()
        .flat_map(|f| f.nav().into_iter().map(move |(label, path)| serde_json::json!({"feature": f.name(), "label": label, "path": path})))
        .collect();
    let mut api = Router::new()
        .merge(auth::routes())
        .merge(events::routes())
        .route("/health", axum::routing::get(|| async { axum::Json(serde_json::json!({"ok": true})) }))
        .route(
            "/meta",
            axum::routing::get(move |_u: User, axum::extract::State(s): axum::extract::State<AppState>| {
                let nav = nav.clone();
                async move { axum::Json(serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "features": s.core.features, "nav": nav})) }
            }),
        );
    for f in features {
        api = api.nest(&format!("/{}", f.name()), f.routes());
    }
    Router::new()
        .nest("/api/v1", api)
        .route(
            "/health",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        )
        .merge(static_files::routes())
        .with_state(AppState {
            core,
            features: Arc::new(features.to_vec()),
        })
}
