//! SSE 事件串流：`GET /api/v1/events`，以 `Last-Event-ID` header 或 `?after=` 從指定序號之後補送，不漏事件。
//! 篩選：`?kinds=chat.,runtime.` 只收這些前綴。

use crate::auth::User;
use crate::AppState;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::Router;
use futures::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;

#[derive(Deserialize, Default)]
pub struct EventQuery {
    pub after: Option<u64>,
    pub kinds: Option<String>,
}

async fn stream(
    State(s): State<AppState>,
    u: User,
    headers: HeaderMap,
    Query(q): Query<EventQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let from_header = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let after = from_header.or(q.after);
    let prefixes: Vec<String> = q
        .kinds
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    let mut sub = s.core.bus.subscribe();
    if let Some(a) = after {
        sub = sub.from_seq(a);
    }
    let st = futures::stream::unfold(
        (sub, prefixes, s, u),
        |(mut sub, prefixes, s, u)| async move {
            loop {
                let ev = sub.next().await.ok()?;
                if !prefixes.is_empty() && !prefixes.iter().any(|p| ev.kind.starts_with(p)) {
                    continue;
                }
                if !s.features.iter().all(|f| f.can_see(&s, &u, &ev)) {
                    continue;
                }
                let data = serde_json::to_string(&ev).unwrap_or_default();
                // 不設 event 名稱：瀏覽器 onmessage 統一接收，kind 在 data 內
                let item = Event::default().id(ev.seq.to_string()).data(data);
                return Some((Ok(item), (sub, prefixes, s, u)));
            }
        },
    );
    Sse::new(st).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/events", get(stream))
}
