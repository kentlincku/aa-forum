use aaf_core::{feature::BootOptions, storage::Db, Core, Registry};
use aaf_feature_context::Context;
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn get(app: &Router, token: &str, path: &str) -> (u16, Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = to_bytes(r.into_body(), 1_000_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
fn app(c: Arc<Core>) -> Router {
    let h = aaf_feature_context::http::ContextHttp::new(Arc::new(|core, u, lane| {
        core.db.with(|c| {
            let r = aaf_feature_chat::store::room(c, lane.room.unwrap())?;
            Ok(r.is_some_and(|r| r.state != "deleted")
                && (u.rank == aaf_core::perms::Rank::Owner
                    || aaf_feature_chat::store::is_member(c, lane.room.unwrap(), &u.name)?))
        })
    }));
    aaf_api::app(c, &[Arc::new(h)])
}

#[tokio::test]
async fn source_api_paginates_full_old_decisions_with_lane_permissions() {
    let home = tempfile::tempdir().unwrap();
    let mut reg = Registry::default();
    reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
    reg.register(Box::new(aaf_feature_chat::Chat::new(
        aaf_runtime::dispatcher::Roster {
            instance: home.path().into(),
            ..Default::default()
        },
    )))
    .unwrap();
    reg.register(Box::new(Context)).unwrap();
    let core = Arc::new(
        Core::prepare(
            &reg,
            Db::open_in_memory().unwrap(),
            BootOptions {
                env: vec![],
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let old = json!({"body":format!("EARLY-CANARY {} final requirement", "決策𠮷".repeat(7000))})
        .to_string();
    core.db.with(|c| {
        c.execute("INSERT INTO chat_rooms(id,name,created_by,created_at) VALUES (7,'A','owner',0),(8,'B','owner',0)", [])?;
        c.execute("INSERT INTO chat_members(room,member,kind,added_at) VALUES (7,'writer','role',0),(7,'reviewer','role',0)", [])?;
        c.execute("INSERT INTO context_sources(role,room,kind,body) VALUES ('writer',7,'input',?1)", [&old])?;
        for i in 0..700 {
            c.execute("INSERT INTO context_sources(role,room,kind,body) VALUES ('writer',7,'input',?1)", [json!({"body":format!("later {i}")}).to_string()])?;
        }
        c.execute("INSERT INTO context_sources(role,room,kind,body) VALUES ('writer',8,'input','{\"body\":\"PRIVATE\"}')", [])?;
        Ok(())
    }).unwrap();
    // Raw tool results must not leak through the room-wide SSE channel.
    {
        use aaf_api::HttpFeature;
        let chat = aaf_feature_chat::Chat::new(aaf_runtime::dispatcher::Roster {
            instance: home.path().into(),
            ..Default::default()
        });
        let state = aaf_api::AppState {
            core: core.clone(),
            features: Arc::new(vec![]),
        };
        let event=core.bus.publish("runtime.agent.tool_evidence",1,json!({"lane":{"role":"writer","room":7},"event":{"type":"tool_evidence","data":{"id":"x","output":"RAW-PRIVATE"}}})).unwrap();
        for (name, rank) in [
            ("writer", aaf_core::perms::Rank::Worker),
            ("reviewer", aaf_core::perms::Rank::Lead),
            ("owner", aaf_core::perms::Rank::Owner),
        ] {
            assert!(
                !chat.can_see(
                    &state,
                    &aaf_api::User {
                        name: name.into(),
                        rank
                    },
                    &event
                ),
                "raw tool evidence is explicit source retrieval only, not SSE"
            );
        }
    }
    let token = aaf_api::issue_role_token(&core, "writer", "worker").unwrap();
    let reviewer = aaf_api::issue_role_token(&core, "reviewer", "lead").unwrap();
    let app = app(core.clone());
    let setup = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/setup")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"name":"owner","password":"test-only-password"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(setup.status().as_u16(), 200);
    let owner: Value =
        serde_json::from_slice(&to_bytes(setup.into_body(), 10000).await.unwrap()).unwrap();
    let owner_token = owner["token"].as_str().unwrap();
    let root = "/api/v1/context/rooms/7/roles/writer/sources";
    assert_eq!(
        get(&app, owner_token, root).await.0,
        200,
        "owner may explicitly inspect an existing lane"
    );
    let (status, first) = get(&app, &token, root).await;
    assert_eq!(
        status, 200,
        "agent must have an authenticated durable source API"
    );
    assert_eq!(first["sources"][0]["id"], 1);
    assert!(first["sources"][0]["excerpt"]
        .as_str()
        .unwrap()
        .contains("EARLY-CANARY"));
    let mut offset = 0;
    let mut reconstructed = String::new();
    loop {
        let (status, page) = get(&app, &token, &format!("{root}?source=1&offset={offset}")).await;
        assert_eq!(status, 200);
        reconstructed.push_str(page["text"].as_str().unwrap());
        if page["next_offset"].is_null() {
            break;
        }
        let next = page["next_offset"].as_u64().unwrap();
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(reconstructed, old);
    assert_eq!(get(&app, "invalid", root).await.0, 401);
    assert_eq!(get(&app, &reviewer, root).await.0, 403);
    assert_eq!(
        get(&app, &token, "/api/v1/context/rooms/8/roles/writer/sources")
            .await
            .0,
        403
    );
    assert_eq!(
        get(&app, &token, &format!("{root}?source=702")).await.0,
        404
    );
    assert_eq!(get(&app, &token, &format!("{root}?after=-1")).await.0, 400);
    assert_eq!(
        get(&app, &token, &format!("{root}?source=1&offset=999999"))
            .await
            .0,
        400
    );
    core.db
        .with(|c| {
            c.execute("DELETE FROM chat_members WHERE member='writer'", [])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(get(&app, &token, root).await.0, 403);
}
#[tokio::test]
async fn source_pages_honor_nondefault_byte_limit_and_reject_nonboundary_offsets() {
    let mut reg = Registry::default();
    reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
    reg.register(Box::new(Context)).unwrap();
    let core = Arc::new(
        Core::prepare(
            &reg,
            Db::open_in_memory().unwrap(),
            BootOptions {
                env: vec![
                    ("AAF_CONTEXT_PAGE_BYTES".into(), "4".into()),
                    ("AAF_CONTEXT_ENABLED".into(), "0".into()),
                ],
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let body = json!({"v":"𠮷中🦀"}).to_string();
    core.db
        .with(|c| {
            c.execute(
                "INSERT INTO context_sources(role,room,kind,body) VALUES ('writer',7,'input',?1)",
                [&body],
            )?;
            Ok(())
        })
        .unwrap();
    let token = aaf_api::issue_role_token(&core, "writer", "worker").unwrap();
    let http = aaf_feature_context::http::ContextHttp::new(Arc::new(|_, _, _| Ok(true)));
    let app = aaf_api::app(core, &[Arc::new(http)]);
    let root = "/api/v1/context/rooms/7/roles/writer/sources?source=1";
    let mut offset = 0;
    let mut text = String::new();
    loop {
        let (status, v) = get(&app, &token, &format!("{root}&offset={offset}")).await;
        assert_eq!(status, 200);
        let part = v["text"].as_str().unwrap();
        assert!(part.len() <= 4);
        text.push_str(part);
        let Some(next) = v["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(text, body);
    let bad = body.find('𠮷').unwrap() + 1;
    assert_eq!(
        get(&app, &token, &format!("{root}&offset={bad}")).await.0,
        400
    );
}
