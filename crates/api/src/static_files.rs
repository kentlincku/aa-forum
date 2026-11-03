//! 內嵌前端：`web/dist` 編進執行檔，`/app/*` 提供，未知路徑回 index.html（SPA）。

use crate::AppState;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist"]
struct Assets;

fn serve(path: &str) -> Response {
    let (file, name) = match Assets::get(path) {
        Some(f) => (f, path),
        None => match Assets::get("index.html") {
            Some(f) => (f, "index.html"),
            None => return (StatusCode::NOT_FOUND, "frontend not built").into_response(),
        },
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, "default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'".to_string()),
        ],
        file.data,
    )
        .into_response()
}

async fn app_path(uri: Uri) -> Response {
    let p = uri
        .path()
        .trim_start_matches("/app")
        .trim_start_matches('/');
    serve(if p.is_empty() { "index.html" } else { p })
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/app/") }))
        .route("/app", get(|| async { Redirect::temporary("/app/") }))
        .route("/app/", get(app_path))
        .route("/app/{*path}", get(app_path))
}
