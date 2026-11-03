use aaf_core::{feature::BootOptions, storage::Db, Core, Registry};
use aaf_feature_chat::{store, Chat};
use aaf_feature_context::Context;
use aaf_runtime::{
    dispatcher::{Lane, RoleSpec, Roster},
    DriverSpec,
};

#[tokio::test]
async fn chat_inbox_installs_context_and_advertises_retrieval_without_duplicate_packages() {
    let home = tempfile::tempdir().unwrap();
    let mut roster = Roster {
        instance: home.path().into(),
        ..Default::default()
    };
    roster.roles.insert(
        "writer".into(),
        RoleSpec::new(
            DriverSpec::Command {
                command: vec!["/bin/cat".into()],
                cwd: None,
            },
            home.path(),
        ),
    );
    let chat = Chat::new(roster.clone());
    chat.set_continuity(aaf_feature_context::runtime);
    let shared = chat.shared();
    let mut reg = Registry::default();
    reg.register(Box::new(aaf_api::AuthFeature)).unwrap();
    reg.register(Box::new(chat)).unwrap();
    reg.register(Box::new(Context)).unwrap();
    let core = Core::prepare(
        &reg,
        Db::open_in_memory().unwrap(),
        BootOptions {
            env: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    core.db.with(|c|{c.execute("INSERT INTO auth_users(name,rank,pw_hash,created_at) VALUES ('owner','owner','unused-test-hash',0)",[])?;Ok(())}).unwrap();
    let room = store::write(&core, |tx, events| {
        store::create_room(tx, events, &roster, "test", "owner", &["writer".into()])
    })
    .unwrap();
    let post = |text: &str| {
        store::write(&core, |tx, events| {
            store::post_message(tx, events, &roster, room.id, "owner", "user", text, None)
        })
        .unwrap()
    };
    let d = shared.dispatcher(&core);
    post("CHAT-CANARY next action: inspect generated artifact");
    d.once().await.unwrap();
    let first: String = core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT body FROM chat_messages WHERE author_kind='role' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(
        first.contains("aaf context read"),
        "new agents need to discover source retrieval before any recovery"
    );
    post("ordinary");
    d.once().await.unwrap();
    let second: String = core
        .db
        .with(|c| {
            Ok(c.query_row(
                "SELECT body FROM chat_messages WHERE author_kind='role' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(!second.contains("AAF CONTINUATION"));
    let mut changed = roster.clone();
    changed.roles.get_mut("writer").unwrap().driver = DriverSpec::Command {
        command: vec!["/bin/sh".into(), "-c".into(), "cat".into()],
        cwd: None,
    };
    d.apply_roster(changed).await.unwrap();
    post("continue");
    d.once().await.unwrap();
    core.db
        .with(|c| {
            let reply: String = c.query_row(
                "SELECT body FROM chat_messages WHERE author_kind='role' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            assert!(reply.contains("CHAT-CANARY"));
            let queued: i64 = c.query_row(
                "SELECT count(*) FROM chat_deliveries WHERE state='queued'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(queued, 0);
            let versions: i64 =
                c.query_row("SELECT count(*) FROM context_checkpoints", [], |r| r.get(0))?;
            assert_eq!(versions, 3);
            Ok(())
        })
        .unwrap();
    d.close(&Lane::new("writer", Some(room.id)), false)
        .await
        .unwrap();
}
