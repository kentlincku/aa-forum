//! API 整合測試：在隨機埠啟動真實伺服器，以 HTTP 驗證登入、權限、SSE 補送、路由註冊、前端。

use aaf_api::{AppState, AuthFeature, HttpFeature, User};
use aaf_core::feature::BootOptions;
use aaf_core::perms::{CapabilityDef, Rank};
use aaf_core::storage::Db;
use aaf_core::{Core, Feature, Registry};
use axum::routing::get;
use axum::{Json, Router};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Demo;
const CAPS: &[CapabilityDef] = &[CapabilityDef {
    id: "demo.secret.read",
    description: "",
    default_ranks: &[Rank::Human],
}];
impl Feature for Demo {
    fn name(&self) -> &'static str {
        "demo"
    }
    fn capabilities(&self) -> &'static [CapabilityDef] {
        CAPS
    }
}
impl HttpFeature for Demo {
    fn name(&self) -> &'static str {
        "demo"
    }
    fn routes(&self) -> Router<AppState> {
        Router::new().route(
            "/secret",
            get(
                |axum::extract::State(s): axum::extract::State<AppState>, u: User| async move {
                    u.require(&s, "demo.secret.read")?;
                    s.core
                        .bus
                        .publish("demo.secret.read", 1, serde_json::json!({"by": u.name}))?;
                    Ok::<_, aaf_api::ApiError>(Json(serde_json::json!({"secret": 42})))
                },
            ),
        )
    }
    fn nav(&self) -> Vec<(&'static str, &'static str)> {
        vec![("Demo", "/demo")]
    }
}

async fn start() -> (String, Arc<Core>) {
    let mut reg = Registry::default();
    reg.register(Box::new(AuthFeature)).unwrap();
    reg.register(Box::new(Demo)).unwrap();
    let core = Core::boot(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    let app = aaf_api::app(core.clone(), &[Arc::new(Demo)]);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, core)
}

/// 極簡 HTTP/1.1 用戶端（不額外引入 reqwest）。
async fn req(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (u16, String, serde_json::Value) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let b = body.map(|v| v.to_string()).unwrap_or_default();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let r = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len());
    s.write_all(r.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    // chunked 時取第一個 JSON 物件
    let json = body
        .find('{')
        .and_then(|i| {
            serde_json::Deserializer::from_str(&body[i..])
                .into_iter::<serde_json::Value>()
                .next()?
                .ok()
        })
        .unwrap_or_default();
    (status, head.to_string(), json)
}

fn creds(n: &str, p: &str) -> Option<serde_json::Value> {
    Some(serde_json::json!({"name": n, "password": p}))
}

#[tokio::test]
async fn setup_login_permissions_and_feature_routes() {
    let (a, _core) = start().await;
    assert_eq!(
        req(&a, "GET", "/api/v1/auth/setup", None, None).await.2["needs_setup"],
        true
    );
    assert_eq!(req(&a, "GET", "/api/v1/meta", None, None).await.0, 401);
    assert_eq!(
        req(
            &a,
            "POST",
            "/api/v1/auth/setup",
            None,
            creds("alice", "short")
        )
        .await
        .0,
        400
    );

    let (st, head, body) = req(
        &a,
        "POST",
        "/api/v1/auth/setup",
        None,
        creds("alice", "correct horse"),
    )
    .await;
    assert_eq!(st, 200);
    assert!(head.to_lowercase().contains("set-cookie: aaf_session="));
    assert!(head.contains("HttpOnly") && head.contains("SameSite=Strict"));
    let owner = body["token"].as_str().unwrap().to_string();
    assert_eq!(
        req(
            &a,
            "POST",
            "/api/v1/auth/setup",
            None,
            creds("eve", "correct horse")
        )
        .await
        .0,
        409,
        "setup only once"
    );

    assert_eq!(
        req(
            &a,
            "POST",
            "/api/v1/auth/login",
            None,
            creds("alice", "wrong password")
        )
        .await
        .0,
        401
    );
    assert_eq!(
        req(
            &a,
            "POST",
            "/api/v1/auth/login",
            None,
            creds("nobody", "whatever1")
        )
        .await
        .0,
        401
    );
    let t2 = req(
        &a,
        "POST",
        "/api/v1/auth/login",
        None,
        creds("alice", "correct horse"),
    )
    .await
    .2["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        req(&a, "GET", "/api/v1/auth/me", Some(&t2), None).await.2["rank"],
        "owner"
    );

    let meta = req(&a, "GET", "/api/v1/meta", Some(&owner), None).await.2;
    assert_eq!(meta["nav"][0]["path"], "/demo");
    assert_eq!(meta["features"], serde_json::json!(["auth", "demo"]));

    // feature 路由：/api/v1/demo/secret；Owner 可讀
    assert_eq!(
        req(&a, "GET", "/api/v1/demo/secret", Some(&owner), None)
            .await
            .2["secret"],
        42
    );

    // Owner 建 human；human 有 demo.secret.read 但沒有 auth.user.create
    assert_eq!(
        req(
            &a,
            "POST",
            "/api/v1/auth/users",
            Some(&owner),
            creds("ann", "password123")
        )
        .await
        .0,
        200
    );
    assert_eq!(
        req(
            &a,
            "POST",
            "/api/v1/auth/users",
            Some(&owner),
            creds("ann", "password123")
        )
        .await
        .0,
        409
    );
    let ann = req(
        &a,
        "POST",
        "/api/v1/auth/login",
        None,
        creds("ann", "password123"),
    )
    .await
    .2["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        req(&a, "GET", "/api/v1/demo/secret", Some(&ann), None)
            .await
            .0,
        200
    );
    assert_eq!(
        req(
            &a,
            "POST",
            "/api/v1/auth/users",
            Some(&ann),
            creds("bob", "password123")
        )
        .await
        .0,
        403
    );

    // 登出後 token 失效
    assert_eq!(
        req(&a, "POST", "/api/v1/auth/logout", Some(&ann), None)
            .await
            .0,
        200
    );
    assert_eq!(
        req(&a, "GET", "/api/v1/auth/me", Some(&ann), None).await.0,
        401
    );
    assert_eq!(
        req(&a, "GET", "/api/v1/auth/me", Some("garbage"), None)
            .await
            .0,
        401
    );
}

/// 讀 SSE 直到收到 n 個 data 行。
async fn sse(
    addr: &str,
    token: &str,
    path: &str,
    last_id: Option<u64>,
    n: usize,
) -> Vec<serde_json::Value> {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let lid = last_id
        .map(|i| format!("Last-Event-ID: {i}\r\n"))
        .unwrap_or_default();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n{lid}Accept: text/event-stream\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = String::new();
    let mut chunk = [0u8; 4096];
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while out.len() < n {
            let k = s.read(&mut chunk).await.unwrap();
            buf.push_str(&String::from_utf8_lossy(&chunk[..k]));
            while let Some(i) = buf.find("data: ") {
                let rest = &buf[i + 6..];
                let Some(j) = rest.find('\n') else { break };
                out.push(serde_json::from_str(&rest[..j]).unwrap());
                buf = rest[j..].to_string();
            }
        }
    })
    .await
    .expect("sse events in time");
    out
}

#[tokio::test]
async fn sse_replays_after_last_event_id_and_filters() {
    let (a, core) = start().await;
    let t = req(
        &a,
        "POST",
        "/api/v1/auth/setup",
        None,
        creds("alice", "correct horse"),
    )
    .await
    .2["token"]
        .as_str()
        .unwrap()
        .to_string();
    // seq 1 = auth.user.created
    for i in 0..3 {
        core.bus
            .publish("demo.tick", 1, serde_json::json!({"i": i}))
            .unwrap();
    }
    // 從 seq 2 之後補送：應拿到 i=1、i=2
    let got = sse(&a, &t, "/api/v1/events?kinds=demo.", Some(2), 2).await;
    assert_eq!(
        got.iter()
            .map(|e| e["payload"]["i"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    // 即時：訂閱後再發
    let a2 = a.clone();
    let t2 = t.clone();
    let live =
        tokio::spawn(
            async move { sse(&a2, &t2, "/api/v1/events?kinds=demo.secret", None, 1).await },
        );
    tokio::time::sleep(Duration::from_millis(200)).await;
    core.bus
        .publish("demo.tick", 1, serde_json::json!({}))
        .unwrap(); // 被過濾
    assert_eq!(
        req(&a, "GET", "/api/v1/demo/secret", Some(&t), None)
            .await
            .0,
        200
    );
    let got = live.await.unwrap();
    assert_eq!(got[0]["kind"], "demo.secret.read");
    assert_eq!(got[0]["payload"]["by"], "alice");
    // 未登入不可訂閱
    assert_eq!(req(&a, "GET", "/api/v1/events", None, None).await.0, 401);
}

#[tokio::test]
async fn serves_embedded_frontend() {
    let (a, _) = start().await;
    let (st, head, _) = req(&a, "GET", "/", None, None).await;
    assert_eq!(st, 307);
    assert!(head.to_lowercase().contains("location: /app/"));
    let mut s = tokio::net::TcpStream::connect(&a).await.unwrap();
    s.write_all(b"GET /app/rooms/1 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"));
    assert!(
        out.contains("<div id=\"root\">"),
        "SPA fallback to index.html"
    );
    assert!(out.to_lowercase().contains("content-security-policy"));
}
