//! Driver 與 dispatcher 的端到端測試（假 ACP agent、echo 指令，不連模型）。

use aaf_core::events::EventBus;
use aaf_core::storage::Db;
use aaf_runtime::acp::AcpDriver;
use aaf_runtime::command::CommandDriver;
use aaf_runtime::dispatcher::{Dispatcher, Inbox, InboxMessage, Lane, RoleSpec, Roster};
use aaf_runtime::manual::ManualDriver;
use aaf_runtime::{AgentStatus, Driver, DriverEvent, DriverSpec, PermissionDecision};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn fake_acp() -> AcpDriver {
    AcpDriver::new(
        vec![env!("CARGO_BIN_EXE_aaf-fake-acp").into()],
        std::env::temp_dir(),
        vec![],
    )
}

async fn next_matching(
    rx: &mut tokio::sync::broadcast::Receiver<DriverEvent>,
    f: impl Fn(&DriverEvent) -> bool,
) -> DriverEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let e = rx.recv().await.unwrap();
            if f(&e) {
                return e;
            }
        }
    })
    .await
    .expect("event in time")
}

#[tokio::test]
async fn acp_prompt_streams_and_returns_text() {
    let d = fake_acp();
    assert_eq!(d.status(), AgentStatus::Offline);
    d.start().await.unwrap();
    assert_eq!(d.status(), AgentStatus::Idle);
    let mut rx = d.events();
    let r = d.prompt("hello").await.unwrap();
    assert_eq!(r.text, "ECHO: hello");
    assert_eq!(r.stop_reason, "end_turn");
    let mut chunks = String::new();
    while let Ok(e) = rx.try_recv() {
        if let DriverEvent::MessageChunk { text } = e {
            chunks.push_str(&text);
        }
    }
    assert_eq!(chunks, "ECHO: hello");
    assert_eq!(d.status(), AgentStatus::Idle);
    d.stop().await.unwrap();
    assert_eq!(d.status(), AgentStatus::Offline);
}

#[tokio::test]
async fn acp_reports_context_usage_and_clears_it_on_restart() {
    let d = fake_acp();
    d.start().await.unwrap();
    d.prompt("hello").await.unwrap();
    assert_eq!(d.context_usage(), None, "nothing reported yet");
    d.prompt("USAGE one").await.unwrap();
    d.prompt("USAGE two").await.unwrap();
    let u = d.context_usage().unwrap();
    assert_eq!((u.used, u.size), (3000, 200000), "latest report wins");
    assert_eq!(u.agent, "fake-acp");
    d.stop().await.unwrap();
    d.start().await.unwrap();
    assert_eq!(d.context_usage(), None, "a restart starts unmeasured");
    d.stop().await.unwrap();
}

#[tokio::test]
async fn acp_captures_the_claude_rate_limit_from_usage_updates() {
    let d = fake_acp().with_account(Some("work".into()));
    d.start().await.unwrap();
    d.prompt("USAGE plain").await.unwrap();
    assert!(d.rate_limit().is_none(), "no rate limit info yet");
    d.prompt("USAGE RATELIMIT").await.unwrap();
    let v = d.rate_limit().expect("windows after the update");
    assert_eq!(v["account"], "work");
    assert_eq!(v["windows"][0]["remaining_percent"], 90.0);
    d.stop().await.unwrap();
}

#[tokio::test]
async fn acp_permission_request_blocks_until_answered() {
    let d = Arc::new(fake_acp());
    d.start().await.unwrap();
    let mut rx = d.events();
    let d2 = d.clone();
    let turn = tokio::spawn(async move { d2.prompt("NEED_PERMISSION").await });
    let ev = next_matching(&mut rx, |e| {
        matches!(e, DriverEvent::PermissionRequested { .. })
    })
    .await;
    let DriverEvent::PermissionRequested {
        request_id,
        title,
        options,
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(title, "write file");
    assert_eq!(options.len(), 2);
    assert_eq!(d.status(), AgentStatus::Blocked);
    d.respond_permission(
        request_id,
        PermissionDecision::Selected {
            option_id: "allow".into(),
        },
    )
    .await
    .unwrap();
    let r = turn.await.unwrap().unwrap();
    assert_eq!(r.text, "PERMISSION: allow");
    assert_eq!(d.status(), AgentStatus::Idle);
}

#[tokio::test]
async fn acp_cancel_stops_turn_and_pending_permission() {
    let d = Arc::new(fake_acp());
    d.start().await.unwrap();
    let mut rx = d.events();
    let d2 = d.clone();
    let turn = tokio::spawn(async move { d2.prompt("NEED_PERMISSION").await });
    next_matching(&mut rx, |e| {
        matches!(e, DriverEvent::PermissionRequested { .. })
    })
    .await;
    d.cancel().await.unwrap();
    let r = turn.await.unwrap().unwrap();
    assert_eq!(r.stop_reason, "cancelled");

    let d3 = d.clone();
    let slow = tokio::spawn(async move { d3.prompt("SLOW").await });
    next_matching(&mut rx, |e| {
        matches!(
            e,
            DriverEvent::Status {
                status: AgentStatus::Working
            }
        )
    })
    .await;
    assert!(d.prompt("again").await.is_err(), "busy while working");
    d.cancel().await.unwrap();
    assert_eq!(slow.await.unwrap().unwrap().stop_reason, "cancelled");
    assert_eq!(d.prompt("ok").await.unwrap().text, "ECHO: ok");
}

#[tokio::test]
async fn acp_agent_exit_goes_offline() {
    let d = AcpDriver::new(
        vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        std::env::temp_dir(),
        vec![],
    );
    assert!(d.start().await.is_err());
    assert_eq!(d.status(), AgentStatus::Offline);
}

/// 指令不讀 stdin 就結束（例如 `echo`）：寫入提示時的 EPIPE 不算回合失敗，照常取輸出。
/// 提示比 pipe 緩衝大，讓 macOS 與 Linux 都必定撞到 EPIPE。
#[tokio::test]
async fn command_that_ignores_stdin_still_replies() {
    let d = CommandDriver::new(
        vec!["/bin/sh".into(), "-c".into(), "echo done".into()],
        std::env::temp_dir(),
    );
    d.start().await.unwrap();
    let big = "x".repeat(4 * 1024 * 1024);
    let r = d.prompt(&big).await.unwrap();
    assert_eq!(
        (r.stop_reason.as_str(), r.text.as_str()),
        ("end_turn", "done")
    );
}

#[tokio::test]
async fn command_driver_echo_and_cancel() {
    let d = Arc::new(CommandDriver::new(
        vec!["/bin/cat".into()],
        std::env::temp_dir(),
    ));
    d.start().await.unwrap();
    assert_eq!(d.prompt("hi\n").await.unwrap().text, "hi");
    let slow = Arc::new(CommandDriver::new(
        vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        std::env::temp_dir(),
    ));
    let s2 = slow.clone();
    let t = tokio::spawn(async move { s2.prompt("").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    slow.cancel().await.unwrap();
    assert_eq!(t.await.unwrap().unwrap().stop_reason, "cancelled");
}

#[tokio::test]
async fn manual_is_never_prompted() {
    let d = ManualDriver::default();
    assert!(d.prompt("x").await.is_err());
    assert_eq!(d.status(), AgentStatus::Offline);
}

#[tokio::test]
async fn acp_resumes_session_without_replaying_history_into_reply() {
    let d = fake_acp();
    d.start().await.unwrap();
    let sid = d.session_id().unwrap();
    d.stop().await.unwrap();
    let d2 = fake_acp().with_resume(Some(sid.clone()));
    d2.start().await.unwrap();
    assert_eq!(d2.session_id().as_deref(), Some(sid.as_str()));
    let r = d2.prompt("WHOAMI").await.unwrap();
    assert_eq!(
        r.text,
        format!("{sid} turn 1"),
        "replayed history must not leak into reply"
    );
}

/// 記憶體信箱：模擬 mailbox feature（實作換成 SQLite）。
#[derive(Default)]
struct MemInbox {
    fail_save_once: std::sync::atomic::AtomicBool,
    msgs: Mutex<Vec<Mail>>,
    sessions: Mutex<HashMap<Lane, String>>,
    /// 角色是否每群一條 lane（與 Roster 一致）
    per_room: HashMap<String, bool>,
}

#[derive(Clone, Debug)]
struct Mail {
    id: i64,
    from: String,
    to: String,
    body: String,
    room: Option<i64>,
    thread: Option<i64>,
    acked: bool,
}

impl MemInbox {
    fn lane_of(&self, m: &Mail) -> Lane {
        let per_room = self.per_room.get(&m.to).copied().unwrap_or(true);
        Lane::new(&m.to, if per_room { m.room } else { None })
    }
    fn post(&self, from: &str, to: &str, body: &str, room: Option<i64>) -> i64 {
        let mut v = self.msgs.lock().unwrap();
        let id = v.len() as i64 + 1;
        v.push(Mail {
            id,
            from: from.into(),
            to: to.into(),
            body: body.into(),
            room,
            thread: None,
            acked: false,
        });
        id
    }
    fn all_to(&self, to: &str) -> Vec<Mail> {
        self.msgs
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.to == to)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Inbox for MemInbox {
    async fn lanes_with_unread(&self) -> aaf_core::Result<Vec<Lane>> {
        Ok(self
            .msgs
            .lock()
            .unwrap()
            .iter()
            .filter(|m| !m.acked)
            .map(|m| self.lane_of(m))
            .collect())
    }
    async fn unread(&self, lane: &Lane) -> aaf_core::Result<Vec<InboxMessage>> {
        Ok(self
            .msgs
            .lock()
            .unwrap()
            .iter()
            .filter(|m| !m.acked && &self.lane_of(m) == lane)
            .map(|m| InboxMessage {
                id: m.id,
                from: m.from.clone(),
                body: m.body.clone(),
                room: m.room,
                thread_id: m.thread,
            })
            .collect())
    }
    async fn ack(&self, role: &str, ids: &[i64]) -> aaf_core::Result<()> {
        for m in self
            .msgs
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|m| m.to == role && ids.contains(&m.id))
        {
            m.acked = true;
        }
        Ok(())
    }
    async fn reply(&self, role: &str, to: &InboxMessage, body: &str) -> aaf_core::Result<()> {
        let mut v = self.msgs.lock().unwrap();
        let id = v.len() as i64 + 1;
        v.push(Mail {
            id,
            from: role.into(),
            to: to.from.clone(),
            body: body.into(),
            room: to.room,
            thread: Some(to.thread_id.unwrap_or(to.id)),
            acked: false,
        });
        Ok(())
    }
    async fn notify(&self, room: Option<i64>, body: &str) -> aaf_core::Result<Option<i64>> {
        Ok(Some(self.post("system", "me", body, room)))
    }
    async fn load_session(&self, lane: &Lane) -> aaf_core::Result<Option<String>> {
        Ok(self.sessions.lock().unwrap().get(lane).cloned())
    }
    async fn forget_sessions(&self, role: &str) -> aaf_core::Result<()> {
        self.sessions.lock().unwrap().retain(|l, _| l.role != role);
        Ok(())
    }
    async fn save_session(&self, lane: &Lane, sid: &str) -> aaf_core::Result<()> {
        if self
            .fail_save_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(aaf_core::Error::Other(
                "injected one-shot session persistence failure".into(),
            ));
        }
        self.sessions
            .lock()
            .unwrap()
            .insert(lane.clone(), sid.into());
        Ok(())
    }
}

fn fake_spec() -> DriverSpec {
    DriverSpec::Acp {
        command: vec![env!("CARGO_BIN_EXE_aaf-fake-acp").into()],
        acp_agent: None,
        model: None,
        mode: None,
        cwd: None,
        env: vec![],
        effort: None,
        account: None,
        transport: None,
    }
}

fn roster(entries: &[(&str, DriverSpec, bool)]) -> Roster {
    let mut r = Roster::default();
    for (name, spec, per_room) in entries {
        let mut rs = RoleSpec::new(spec.clone(), std::env::temp_dir());
        rs.room_lanes = *per_room;
        r.roles.insert(name.to_string(), rs);
    }
    r
}

fn setup(entries: &[(&str, DriverSpec, bool)]) -> (Arc<MemInbox>, EventBus, Dispatcher) {
    let inbox = Arc::new(MemInbox {
        per_room: entries
            .iter()
            .map(|(n, _, p)| (n.to_string(), *p))
            .collect(),
        ..Default::default()
    });
    let bus = EventBus::new(Db::open_in_memory().unwrap(), 512);
    let disp = Dispatcher::new(inbox.clone(), bus.clone(), roster(entries));
    (inbox, bus, disp)
}

#[tokio::test]
async fn each_room_gets_its_own_lane_and_session() {
    let (inbox, _bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    inbox.post("me", "coder", "WHOAMI", Some(1));
    inbox.post("me", "coder", "WHOAMI", Some(2));
    inbox.post("me", "coder", "WHOAMI", None);
    let woke = disp.once().await.unwrap();
    assert_eq!(
        woke,
        vec![
            Lane::new("coder", None),
            Lane::new("coder", Some(1)),
            Lane::new("coder", Some(2))
        ]
    );

    let replies = inbox.all_to("me");
    assert_eq!(replies.len(), 3);
    let sids: std::collections::HashSet<_> = replies
        .iter()
        .map(|m| m.body.split(' ').next().unwrap().to_string())
        .collect();
    assert_eq!(
        sids.len(),
        3,
        "three lanes → three distinct sessions: {replies:?}"
    );
    // 回覆沿用原群與討論串
    for r in &replies {
        let orig = inbox
            .msgs
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.to == "coder" && m.room == r.room)
            .cloned()
            .unwrap();
        assert_eq!(r.thread, Some(orig.id));
    }
    assert_eq!(disp.lane_states().await.len(), 3);
}

#[tokio::test]
async fn same_lane_keeps_session_across_turns_and_queues() {
    let (inbox, _bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    inbox.post("me", "coder", "WHOAMI", Some(7));
    disp.once().await.unwrap();
    inbox.post("me", "coder", "WHOAMI", Some(7));
    disp.once().await.unwrap();
    let r: Vec<String> = inbox.all_to("me").into_iter().map(|m| m.body).collect();
    let sid = r[0].split(' ').next().unwrap();
    assert_eq!(r, vec![format!("{sid} turn 1"), format!("{sid} turn 2")]);
}

#[tokio::test]
async fn role_without_room_lanes_uses_single_session() {
    let (inbox, _bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("solo", fake_spec(), false),
    ]);
    inbox.post("me", "solo", "a", Some(1));
    inbox.post("me", "solo", "WHOAMI", Some(2));
    let woke = disp.once().await.unwrap();
    assert_eq!(woke, vec![Lane::new("solo", None)]);
    let r = inbox.all_to("me");
    assert_eq!(r.len(), 1, "both messages handled in one turn");
    assert!(r[0].body.ends_with("turn 1"));
}

#[tokio::test]
async fn parked_lane_resumes_same_session() {
    let (inbox, bus, mut disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    disp.idle_park = Duration::ZERO;
    inbox.post("me", "coder", "WHOAMI", Some(3));
    disp.once().await.unwrap();
    let parked = disp.park_idle().await.unwrap();
    assert_eq!(parked, vec![Lane::new("coder", Some(3))]);
    assert!(disp.lane_states().await.is_empty());
    inbox.post("me", "coder", "WHOAMI", Some(3));
    disp.once().await.unwrap();
    let r: Vec<String> = inbox.all_to("me").into_iter().map(|m| m.body).collect();
    let sid0 = r[0].split(' ').next().unwrap();
    let sid1 = r[1].split(' ').next().unwrap();
    assert_eq!(sid0, sid1, "resumed via session/load: {r:?}");
    assert!(bus
        .since(0, 1000)
        .unwrap()
        .iter()
        .any(|e| e.kind == "runtime.lane.closed"));
}

#[tokio::test]
async fn close_room_with_forget_starts_fresh() {
    let (inbox, _bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    inbox.post("me", "coder", "WHOAMI", Some(4));
    inbox.post("me", "coder", "WHOAMI", Some(5));
    disp.once().await.unwrap();
    let closed = disp.close_room(4, true).await.unwrap();
    assert_eq!(closed, vec![Lane::new("coder", Some(4))]);
    assert_eq!(
        disp.lane_states().await,
        vec![(Lane::new("coder", Some(5)), AgentStatus::Idle)]
    );
    inbox.post("me", "coder", "WHOAMI", Some(4));
    disp.once().await.unwrap();
    let r: Vec<_> = inbox
        .all_to("me")
        .into_iter()
        .filter(|m| m.room == Some(4))
        .map(|m| m.body)
        .collect();
    assert_ne!(
        r[0].split(' ').next(),
        r[1].split(' ').next(),
        "forgotten session must not be resumed"
    );
}

#[tokio::test]
async fn per_room_engine_override_and_mixed_drivers() {
    let mut entries = vec![
        ("me", DriverSpec::Manual, false),
        (
            "echo",
            DriverSpec::Command {
                command: vec!["/usr/bin/tail".into(), "-n1".into()],
                cwd: None,
            },
            true,
        ),
    ];
    entries.push(("coder", fake_spec(), true));
    let (inbox, bus, disp) = setup(&entries);
    // coder 在群 9 改用 echo 指令（每群切換引擎）
    {
        // 每群覆寫走熱重載路徑套用
        let mut r = (*disp.roster()).clone();
        r.roles.get_mut("coder").unwrap().room_overrides.insert(
            9,
            aaf_runtime::dispatcher::RoomOverride {
                full: Some(DriverSpec::Command {
                    command: vec!["/usr/bin/tail".into(), "-n1".into()],
                    cwd: None,
                }),
                ..Default::default()
            },
        );
        disp.apply_roster(r).await.unwrap();
    }
    inbox.post("me", "echo", "ping", None);
    inbox.post("me", "coder", "hello", Some(8));
    inbox.post("me", "coder", "override", Some(9));
    disp.once().await.unwrap();
    let mut bodies: Vec<_> = inbox
        .all_to("me")
        .into_iter()
        .map(|m| (m.from, m.room, m.body))
        .collect();
    bodies.sort();
    assert_eq!(
        bodies,
        vec![
            ("coder".into(), Some(8), "ECHO: hello".into()),
            ("coder".into(), Some(9), "override".into()),
            ("echo".into(), None, "ping".into())
        ]
    );
    assert!(
        disp.once().await.unwrap().is_empty(),
        "nothing left; manual never woken"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let evs = bus.since(0, 1000).unwrap();
    for k in [
        "runtime.turn.started",
        "runtime.agent.status",
        "runtime.agent.stream",
        "runtime.turn.ended",
    ] {
        assert!(evs.iter().any(|e| e.kind == k), "missing {k}");
    }
    assert!(evs
        .iter()
        .any(|e| e.kind == "runtime.turn.started" && e.payload["lane"]["room"] == 8));
}

#[tokio::test]
async fn cancel_targets_one_lane_only() {
    let (inbox, _bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    let disp = Arc::new(disp);
    inbox.post("me", "coder", "SLOW", Some(1));
    inbox.post("me", "coder", "fast", Some(2));
    let d2 = disp.clone();
    let run = tokio::spawn(async move { d2.once().await });
    for _ in 0..200 {
        if disp
            .lane_states()
            .await
            .iter()
            .any(|(l, s)| l.room == Some(1) && *s == AgentStatus::Working)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(disp.cancel(&Lane::new("coder", Some(1))).await.unwrap());
    run.await.unwrap().unwrap();
    let r: Vec<_> = inbox
        .all_to("me")
        .into_iter()
        .filter(|m| m.from != "system")
        .map(|m| (m.room, m.body))
        .collect();
    assert_eq!(
        r,
        vec![(Some(2), "ECHO: fast".to_string())],
        "room 1 cancelled (no reply), room 2 unaffected"
    );
    // 停止公告只出在被停止的那一群（AAF_STOP_NOTICE）
    let n: Vec<_> = notices(&inbox).into_iter().map(|m| m.room).collect();
    assert_eq!(n, vec![Some(1)]);
    assert!(
        inbox
            .lanes_with_unread()
            .await
            .unwrap()
            .iter()
            .all(|l| l.role != "coder"),
        "cancelled turn is acked"
    );
}

#[tokio::test]
async fn roster_change_drains_busy_lane_then_applies_with_fresh_session() {
    let (inbox, bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    let disp = Arc::new(disp);
    // 先跑一輪建立 session
    inbox.post("me", "coder", "WHOAMI", Some(1));
    disp.once().await.unwrap();
    let first_sid = inbox.all_to("me")[0]
        .body
        .split(' ')
        .next()
        .unwrap()
        .to_string();

    // 開始一個慢回合
    inbox.post("me", "coder", "SLOW", Some(1));
    let d2 = disp.clone();
    let run = tokio::spawn(async move { d2.once().await });
    for _ in 0..200 {
        if disp.busy(&Lane::new("coder", Some(1))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // 熱重載：coder 改成 echo 指令
    let mut r = (*disp.roster()).clone();
    r.roles.get_mut("coder").unwrap().driver = DriverSpec::Command {
        command: vec!["/usr/bin/tail".into(), "-n1".into()],
        cwd: None,
    };
    let diff = disp.apply_roster(r).await.unwrap();
    assert_eq!(diff.changed, vec!["coder"]);
    assert_eq!(
        disp.draining(),
        vec!["coder"],
        "busy lane keeps the role draining"
    );
    assert_eq!(disp.lane_states().await.len(), 1, "busy lane is not killed");

    // 收尾期間的新信不派工
    inbox.post("me", "coder", "during drain", Some(1));
    disp.tick().await.unwrap();
    assert!(inbox.all_to("me").iter().all(|m| m.body != "during drain"));

    // 停止慢回合 → reconcile 關閉舊 lane、解除 draining
    disp.cancel(&Lane::new("coder", Some(1))).await.unwrap();
    run.await.unwrap().unwrap();
    assert_eq!(disp.reconcile().await.unwrap(), vec!["coder"]);
    assert!(disp.draining().is_empty());
    assert!(
        inbox
            .load_session(&Lane::new("coder", Some(1)))
            .await
            .unwrap()
            .is_none_or(|s| s.is_empty()),
        "old session forgotten"
    );

    // 新設定生效：echo 回原文
    disp.once().await.unwrap();
    let last = inbox.all_to("me").last().unwrap().body.clone();
    assert_eq!(last, "during drain");
    assert_ne!(last.split(' ').next().unwrap(), first_sid);
    let kinds: Vec<String> = bus
        .since(0, 2000)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(
        kinds.contains(&"runtime.roster.reloaded".into())
            && kinds.contains(&"runtime.roster.applied".into())
    );
}

#[tokio::test]
async fn roster_add_and_remove_roles() {
    let (inbox, _bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    inbox.post("me", "coder", "hi", None);
    disp.once().await.unwrap();
    let mut r = (*disp.roster()).clone();
    r.roles.remove("coder");
    r.roles.insert(
        "helper".into(),
        RoleSpec::new(fake_spec(), std::env::temp_dir()),
    );
    let d = disp.apply_roster(r).await.unwrap();
    assert_eq!(
        (d.added.clone(), d.removed.clone()),
        (vec!["helper".to_string()], vec!["coder".to_string()])
    );
    assert!(
        disp.draining().is_empty(),
        "idle removed role closes immediately"
    );
    assert!(disp.lane_states().await.is_empty());
    // 移除的角色收到的信不會派工
    inbox.post("me", "coder", "anyone?", None);
    assert!(disp.once().await.unwrap().is_empty());
    // 新角色可用
    inbox.post("me", "helper", "hello", None);
    assert_eq!(disp.once().await.unwrap(), vec![Lane::new("helper", None)]);
    // 無變更
    assert!(disp
        .apply_roster((*disp.roster()).clone())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn context_sent_once_per_lane_and_on_change() {
    // head -n1：agent 回覆＝它收到的提示第一行，藉此觀察是否帶了長內容
    let head = DriverSpec::Command {
        command: vec!["/usr/bin/head".into(), "-n1".into()],
        cwd: None,
    };
    let (inbox, _bus, disp) = setup(&[("me", DriverSpec::Manual, false), ("coder", head, true)]);
    let version = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let v = version.clone();
    let disp = disp.with_context(Arc::new(move |lane: &Lane| {
        let n = v.load(std::sync::atomic::Ordering::SeqCst);
        (lane.role == "coder").then(|| (n, format!("SKILLS v{n}\n")))
    }));
    let reply = |inbox: &MemInbox| inbox.all_to("me").last().unwrap().body.clone();
    inbox.post("me", "coder", "one", Some(1));
    disp.once().await.unwrap();
    assert_eq!(reply(&inbox), "SKILLS v1");
    inbox.post("me", "coder", "two", Some(1));
    disp.once().await.unwrap();
    assert_ne!(reply(&inbox), "SKILLS v1", "not resent while unchanged");
    // 別的群＝別的 lane：第一次也要送
    inbox.post("me", "coder", "three", Some(2));
    disp.once().await.unwrap();
    assert_eq!(reply(&inbox), "SKILLS v1");
    // 內容變了：再送一次
    version.store(2, std::sync::atomic::Ordering::SeqCst);
    inbox.post("me", "coder", "four", Some(1));
    disp.once().await.unwrap();
    assert_eq!(reply(&inbox), "SKILLS v2");
}

/// Keep the dispatcher and its LaneState alive: only the ACP child is restarted.
#[tokio::test]
async fn assigned_skills_reinjected_on_in_place_session_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let driver = Arc::new(AcpDriver::new(
        vec![env!("CARGO_BIN_EXE_aaf-fake-acp").into()],
        tmp.path().into(),
        vec![
            ("FAKE_ECHO_PROMPT".into(), "1".into()),
            ("FAKE_FAIL_LOAD".into(), "1".into()),
        ],
    ));
    let (inbox, bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    let drv = driver.clone();
    let disp = disp
        .with_factory(move |_| Some(drv.clone()))
        .with_context(Arc::new(|_| Some((42, "ASSIGNED-SKILL-CANARY\n".into()))));
    let lane = Lane::new("coder", Some(1));
    inbox.post("me", "coder", "first", lane.room);
    disp.once().await.unwrap();
    assert!(inbox.all_to("me")[0].body.contains("ASSIGNED-SKILL-CANARY"));
    let first_session = driver.session_id();
    inbox.post("me", "coder", "next", lane.room);
    disp.once().await.unwrap();
    assert!(!inbox.all_to("me")[1].body.contains("ASSIGNED-SKILL-CANARY"));

    driver.stop().await.unwrap();
    assert_eq!(
        disp.lane_states().await,
        vec![(lane.clone(), AgentStatus::Offline)]
    );
    inbox.post("me", "coder", "after restart", lane.room);
    disp.once().await.unwrap();
    assert_ne!(driver.session_id(), first_session);
    assert!(!driver.session_loaded());
    let reply = inbox.all_to("me")[2].body.clone();
    driver.stop().await.unwrap();
    assert!(
        reply.contains("ASSIGNED-SKILL-CANARY"),
        "new session lost assigned skills: {reply}"
    );
    assert!(!bus
        .since(0, 1000)
        .unwrap()
        .iter()
        .any(|e| e.kind == "runtime.lane.closed"));
}

#[tokio::test]
async fn assigned_skills_dead_restart_load_success_preserves_context() {
    assigned_skills_dead_restart(true, false).await;
}

#[tokio::test]
async fn assigned_skills_dead_restart_load_fallback_resends_context() {
    assigned_skills_dead_restart(false, false).await;
}

#[tokio::test]
async fn assigned_skills_dead_restart_failed_start_then_fallback() {
    assigned_skills_dead_restart(false, true).await;
}

#[tokio::test]
async fn assigned_skills_dead_restart_failed_start_then_load() {
    assigned_skills_dead_restart(true, true).await;
}

async fn assigned_skills_dead_restart(load: bool, fail_start: bool) {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let exe = tmp.path().join("fake-acp");
    symlink(env!("CARGO_BIN_EXE_aaf-fake-acp"), &exe).unwrap();
    let hooks = tmp.path().join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("wake");
    std::fs::write(
        &hook,
        "#!/bin/sh\nif [ \"$AAF_HOOK_PHASE\" = wake ]; then /bin/cat; fi\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    let make_driver = || {
        let mut env = vec![("FAKE_ECHO_PROMPT".into(), "1".into())];
        if !load {
            env.push(("FAKE_FAIL_LOAD".into(), "1".into()));
        }
        Arc::new(AcpDriver::new(
            vec![exe.to_string_lossy().into_owned()],
            tmp.path().into(),
            env,
        ))
    };
    let restarted = make_driver();
    let control = make_driver();
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let (inbox, bus, mut disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    disp.retry_backoff = vec![Duration::ZERO];
    let (r, c, calls) = (restarted.clone(), control.clone(), factory_calls.clone());
    let disp = disp
        .with_factory(move |lane| {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(if lane.room == Some(1) {
                r.clone()
            } else {
                c.clone()
            })
        })
        .with_context(Arc::new(|lane| {
            Some((
                42,
                format!("ASSIGNED-SKILL-CANARY-{}\n", lane.room.unwrap()),
            ))
        }))
        .with_hooks(aaf_runtime::hooks::Hooks::new(hooks, tmp.path()));
    let lane = Lane::new("coder", Some(1));
    let other = Lane::new("coder", Some(2));
    let replies = |room| {
        inbox
            .all_to("me")
            .into_iter()
            .filter(|m| m.from == "coder" && m.room == Some(room))
            .map(|m| m.body)
            .collect::<Vec<_>>()
    };
    let sent = |room| {
        bus.since(0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "runtime.context.sent" && e.payload["lane"]["room"] == room)
            .count()
    };
    for body in ["first", "ordinary next turn"] {
        inbox.post("me", "coder", body, lane.room);
        inbox.post("me", "coder", body, other.room);
        disp.once().await.unwrap();
    }
    for room in [1, 2] {
        let got = replies(room);
        assert_eq!(got.len(), 2);
        assert!(got[0].contains(&format!("ASSIGNED-SKILL-CANARY-{room}")));
        assert!(got[0].contains("\"session_new\":true"));
        assert!(!got[1].contains("ASSIGNED-SKILL-CANARY"));
        assert!(got[1].contains("\"session_new\":false"));
        assert_eq!(sent(room), 1);
    }
    let old_sid = restarted.session_id();
    let control_sid = control.session_id();
    // Kill the real subprocess through its inert fixture command, without closing
    // a lane, dropping the dispatcher, or stopping the driver to clear its conn.
    assert!(restarted.prompt("DIE").await.is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        while restarted.status() != AgentStatus::Offline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(disp.lane_states().await.len(), 2);
    assert_eq!(factory_calls.load(Ordering::SeqCst), 2);
    let pending = inbox.post("me", "coder", "recover pending message", lane.room);
    inbox.post("me", "coder", "control unaffected", other.room);
    if fail_start {
        std::fs::rename(&exe, tmp.path().join("disabled-acp")).unwrap();
        disp.once().await.unwrap();
        assert_eq!(replies(1).len(), 2, "failed start must not consume a turn");
        assert_eq!(sent(1), 1, "failed start must not consume context");
        assert_eq!(inbox.unread(&lane).await.unwrap()[0].id, pending);
        assert_eq!(inbox.load_session(&lane).await.unwrap(), old_sid);
        assert!(bus
            .since(0, 1000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "runtime.lane.start_failed" && e.payload["lane"]["room"] == 1));
        std::fs::rename(tmp.path().join("disabled-acp"), &exe).unwrap();
    }
    disp.once().await.unwrap();
    let recovered_status = restarted.status();
    let recovered_sid = restarted.session_id();
    let loaded = restarted.session_loaded();
    // Explicit cleanup also on the expected pre-fix failure below.
    restarted.stop().await.unwrap();
    control.stop().await.unwrap();
    assert_eq!(
        recovered_status,
        AgentStatus::Idle,
        "dead child must restart in the retained lane"
    );
    assert_eq!(loaded, load);
    assert_eq!(recovered_sid == old_sid, load);
    assert_eq!(control.session_id(), control_sid);
    assert_eq!(replies(1).len(), 3);
    assert_eq!(replies(2).len(), 3);
    let recovered = &replies(1)[2];
    assert_eq!(
        recovered.contains("ASSIGNED-SKILL-CANARY-1"),
        !load,
        "{recovered}"
    );
    assert_eq!(
        recovered.contains("\"session_new\":true"),
        !load,
        "{recovered}"
    );
    assert_eq!(
        recovered.contains("\"session_new\":false"),
        load,
        "{recovered}"
    );
    assert!(!replies(2)[2].contains("ASSIGNED-SKILL-CANARY"));
    assert!(replies(2)[2].contains("\"session_new\":false"));
    assert_eq!(sent(1), if load { 1 } else { 2 });
    assert_eq!(sent(2), 1);
    assert!(inbox.unread(&lane).await.unwrap().is_empty());
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        2,
        "no replacement LaneState"
    );
    assert_eq!(disp.lane_states().await.len(), 2);
    assert!(!bus
        .since(0, 1000)
        .unwrap()
        .iter()
        .any(|e| e.kind == "runtime.lane.closed"));
}

// Independent persistence-failure regression; reviewer inputs/assertions preserved.
#[tokio::test]
async fn review_fresh_session_after_save_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let driver = Arc::new(AcpDriver::new(
        vec![env!("CARGO_BIN_EXE_aaf-fake-acp").into()],
        tmp.path().into(),
        vec![
            ("FAKE_ECHO_PROMPT".into(), "1".into()),
            ("FAKE_FAIL_LOAD".into(), "1".into()),
        ],
    ));
    let (inbox, bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    let drv = driver.clone();
    let disp = disp
        .with_factory(move |_| Some(drv.clone()))
        .with_context(Arc::new(|_| Some((42, "ASSIGNED-SKILL-CANARY\n".into()))));
    let lane = Lane::new("coder", Some(1));
    inbox.post("me", "coder", "first", lane.room);
    disp.once().await.unwrap();
    assert!(inbox.all_to("me")[0].body.contains("ASSIGNED-SKILL-CANARY"));
    let first_session = driver.session_id();
    inbox.post("me", "coder", "next", lane.room);
    disp.once().await.unwrap();
    assert!(!inbox.all_to("me")[1].body.contains("ASSIGNED-SKILL-CANARY"));

    driver.stop().await.unwrap();
    assert_eq!(
        disp.lane_states().await,
        vec![(lane.clone(), AgentStatus::Offline)]
    );
    inbox.post("me", "coder", "after restart", lane.room);
    inbox
        .fail_save_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = disp.once().await.unwrap_err();
    assert!(error.to_string().contains("injected one-shot"));
    assert_eq!(driver.status(), AgentStatus::Idle);
    assert_eq!(inbox.all_to("me").len(), 2);
    assert!(!driver.session_loaded());
    disp.once().await.unwrap();
    assert_ne!(driver.session_id(), first_session);
    assert!(!driver.session_loaded());
    let reply = inbox.all_to("me")[2].body.clone();
    driver.stop().await.unwrap();
    assert!(
        reply.contains("ASSIGNED-SKILL-CANARY"),
        "new session lost assigned skills: {reply}"
    );
    assert!(!bus
        .since(0, 1000)
        .unwrap()
        .iter()
        .any(|e| e.kind == "runtime.lane.closed"));
}

fn fake_model_spec(report: &str, model: Option<&str>) -> DriverSpec {
    DriverSpec::Acp {
        command: vec![env!("CARGO_BIN_EXE_aaf-fake-acp").into()],
        acp_agent: None,
        model: model.map(Into::into),
        mode: None,
        cwd: None,
        env: vec![("FAKE_MODELS".into(), report.into())],
        effort: None,
        account: None,
        transport: None,
    }
}

async fn ask(d: &dyn aaf_runtime::Driver, q: &str) -> String {
    d.prompt(q).await.unwrap().text
}

/// 模型切換：configOptions（set_config_option）與 availableModels（set_model）兩條路，
/// 「清單裡沒有」啟動失敗，agent 不回報模型時照常啟動。
#[tokio::test]
async fn acp_model_switch_paths() {
    let tmp = std::env::temp_dir();
    for report in ["config", "models"] {
        let d = fake_model_spec(report, Some("m2")).build(&tmp);
        d.start().await.unwrap();
        assert_eq!(
            ask(d.as_ref(), "MODEL?").await,
            "MODEL: fake/m2",
            "{report}: suffix match"
        );
        d.stop().await.unwrap();
        let d = fake_model_spec(report, Some("m1")).build(&tmp);
        d.start().await.unwrap();
        assert_eq!(
            ask(d.as_ref(), "MODEL?").await,
            "MODEL: m1",
            "{report}: already current"
        );
        d.stop().await.unwrap();
        let d = fake_model_spec(report, Some("nope")).build(&tmp);
        let e = d.start().await.unwrap_err().to_string();
        assert!(
            e.contains("no model nope") && e.contains("m1, fake/m2"),
            "{report}: {e}"
        );
        assert_eq!(
            d.status(),
            aaf_runtime::AgentStatus::Offline,
            "process stopped after failure"
        );
    }
    let d = fake_model_spec("", Some("whatever")).build(&tmp);
    d.start().await.unwrap();
    assert_eq!(
        ask(d.as_ref(), "MODEL?").await,
        "MODEL: m1",
        "agent without model reporting keeps its default"
    );
    d.stop().await.unwrap();
}

/// 推理強度：依模型回報的選項切換；續接的 session 照常設定；模型不提供的值略過、不擋啟動。
#[tokio::test]
async fn acp_reasoning_effort_follows_the_model() {
    let tmp = std::env::temp_dir();
    let with = |model: &str, effort: Option<&str>| {
        let mut s = fake_model_spec("config", Some(model));
        if let DriverSpec::Acp { effort: e, .. } = &mut s {
            *e = effort.map(Into::into);
        }
        s.build(&tmp)
    };
    let d = with("m1", Some("high"));
    d.start().await.unwrap();
    assert_eq!(ask(d.as_ref(), "EFFORT?").await, "EFFORT: high");
    d.stop().await.unwrap();
    // fake/m2 只有 low／medium：用切換模型後回報的選項判斷，high 不存在就略過
    let d = with("m2", Some("high"));
    d.start().await.unwrap();
    assert_eq!(ask(d.as_ref(), "EFFORT?").await, "EFFORT: medium");
    d.stop().await.unwrap();
    let d = with("m2", Some("low"));
    d.start().await.unwrap();
    assert_eq!(ask(d.as_ref(), "EFFORT?").await, "EFFORT: low");
    d.stop().await.unwrap();
    let d = with("m1", None);
    d.start().await.unwrap();
    assert_eq!(
        ask(d.as_ref(), "EFFORT?").await,
        "EFFORT: medium",
        "agent default"
    );
    d.stop().await.unwrap();
}

/// 帳號：環境變數指到該帳號的資料夾、群覆寫蓋過角色、換 agent 清掉、帳號不存在明確失敗。
#[tokio::test]
async fn account_env_reaches_the_agent_and_overrides_layer() {
    use aaf_runtime::accounts::{save, Account};
    let home = tempfile::tempdir().unwrap();
    let mut m = std::collections::BTreeMap::new();
    for (id, dir) in [("work", "/acct/work"), ("home", "/acct/home")] {
        m.insert(
            id.to_string(),
            Account {
                agent: "claude".into(),
                label: None,
                dir: dir.into(),
            },
        );
    }
    save(home.path(), &m).unwrap();
    let spec = |account: Option<&str>| DriverSpec::Acp {
        command: vec![env!("CARGO_BIN_EXE_aaf-fake-acp").into()],
        acp_agent: Some("claude".into()),
        model: None,
        mode: None,
        cwd: None,
        env: vec![],
        effort: None,
        account: account.map(Into::into),
        transport: None,
    };
    let ctx = aaf_runtime::driver::LaunchContext {
        role: "dev".into(),
        instance: home.path().into(),
        ..Default::default()
    };
    let tmp = std::env::temp_dir();
    let run = |account: Option<&str>| {
        let d = spec(account).build_resuming(&tmp, None, &ctx);
        async move {
            let d = d.unwrap();
            d.start().await.unwrap();
            let r = ask(d.as_ref(), "ENV?").await;
            d.stop().await.unwrap();
            r
        }
    };
    assert_eq!(run(None).await, "ENV: -", "default account sets nothing");
    assert_eq!(run(Some("default")).await, "ENV: -");
    assert_eq!(run(Some("work")).await, "ENV: /acct/work");
    let e = spec(Some("ghost"))
        .build_resuming(&tmp, None, &ctx)
        .err()
        .expect("unknown account must not start");
    assert!(e.contains("account ghost not found"), "{e}");
    let mut wrong = spec(Some("work"));
    if let DriverSpec::Acp { acp_agent, .. } = &mut wrong {
        *acp_agent = Some("codex".into());
    }
    assert!(wrong
        .build_resuming(&tmp, None, &ctx)
        .err()
        .unwrap()
        .contains("is for claude"));

    // 群覆寫蓋過角色；換 agent 會清掉帳號（各家的帳號不通用）
    let base = spec(Some("work"));
    let acct = |o: &aaf_runtime::dispatcher::RoomOverride| match o.apply(&base, "2026-01-01") {
        DriverSpec::Acp { account, .. } => account,
        _ => unreachable!(),
    };
    let mut o = aaf_runtime::dispatcher::RoomOverride::default();
    assert_eq!(
        acct(&o).as_deref(),
        Some("work"),
        "inherits the role account"
    );
    o.account = Some("home".into());
    assert_eq!(acct(&o).as_deref(), Some("home"));
    let other = aaf_runtime::dispatcher::RoomOverride {
        acp_agent: Some("codex".into()),
        ..Default::default()
    };
    assert_eq!(acct(&other), None);
}

#[test]
fn effort_override_layers_and_resets_with_agent() {
    let base = DriverSpec::Acp {
        command: vec![],
        acp_agent: Some("codex".into()),
        model: None,
        mode: None,
        cwd: None,
        env: vec![],
        effort: Some("low".into()),
        account: None,
        transport: None,
    };
    let eff = |o: &aaf_runtime::dispatcher::RoomOverride| match o.apply(&base, "2026-01-01") {
        DriverSpec::Acp { effort, .. } => effort,
        _ => unreachable!(),
    };
    let mut o = aaf_runtime::dispatcher::RoomOverride::default();
    assert_eq!(eff(&o).as_deref(), Some("low"), "inherits the role effort");
    o.effort = Some("high".into());
    assert_eq!(eff(&o).as_deref(), Some("high"));
    let other = aaf_runtime::dispatcher::RoomOverride {
        acp_agent: Some("claude".into()),
        ..Default::default()
    };
    assert_eq!(eff(&other), None, "another agent's effort ids differ");
}

#[tokio::test]
async fn catalog_probe_and_check_model() {
    use aaf_runtime::catalog;
    let tmp = std::env::temp_dir();
    let exe = env!("CARGO_BIN_EXE_aaf-fake-acp").to_string();
    // FAKE_MODELS 透過 env 給：用 /usr/bin/env 包一層
    let cmd = |r: &str| {
        vec![
            "/usr/bin/env".to_string(),
            format!("FAKE_MODELS={r}"),
            exe.clone(),
        ]
    };
    let p = catalog::probe(&cmd("models"), &tmp, Duration::from_secs(10)).await;
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!(p["model"], "m1");
    assert_eq!(p["models"].as_array().unwrap().len(), 3);
    let p = catalog::probe(&cmd("config"), &tmp, Duration::from_secs(10)).await;
    assert_eq!(
        (
            p["effort"].as_str(),
            p["efforts"].as_array().map(|a| a.len())
        ),
        (Some("medium"), Some(3)),
        "{p}"
    );
    let ok = catalog::check_connect(
        &cmd("config"),
        &[],
        &tmp,
        "m1",
        "high",
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        (ok["ok"].as_bool(), ok["effort"].as_str()),
        (Some(true), Some("high")),
        "{ok}"
    );
    // fake/m2 沒有 high：要用切換模型後的選項判斷
    let no = catalog::check_connect(
        &cmd("config"),
        &[],
        &tmp,
        "m2",
        "high",
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(no["ok"], false, "{no}");
    assert!(
        no["error"]
            .as_str()
            .unwrap()
            .contains("no effort high (available: low, medium)"),
        "{no}"
    );
    let c = catalog::check_model(&cmd("config"), &tmp, "m2", Duration::from_secs(10)).await;
    assert_eq!(c["ok"], true, "{c}");
    assert_eq!(c["model"], "fake/m2");
    let bad = catalog::check_model(&cmd("config"), &tmp, "m-broken", Duration::from_secs(10)).await;
    assert_eq!(bad["ok"], false);
    assert!(
        bad["error"].as_str().unwrap().contains("model not found"),
        "{bad}"
    );
    let missing = catalog::check_model(&cmd("models"), &tmp, "zzz", Duration::from_secs(10)).await;
    assert!(missing["error"].as_str().unwrap().contains("no model zzz"));
    let dead = catalog::probe(
        &["/nonexistent/acp".to_string()],
        &tmp,
        Duration::from_secs(5),
    )
    .await;
    assert!(dead["error"].as_str().unwrap().contains("cannot start"));
    // 不回應的 agent：逾時
    let hang = catalog::probe(
        &["/bin/sleep".to_string(), "30".to_string()],
        &tmp,
        Duration::from_millis(500),
    )
    .await;
    assert!(
        hang["error"].as_str().unwrap().contains("timed out"),
        "{hang}"
    );
}

fn layer(
    agent: Option<&str>,
    model: Option<&str>,
    mode: Option<&str>,
    yolo: bool,
    until: Option<&str>,
) -> aaf_runtime::dispatcher::RoomOverride {
    aaf_runtime::dispatcher::RoomOverride {
        full: None,
        acp_agent: agent.map(Into::into),
        model: model.map(Into::into),
        mode: mode.map(Into::into),
        effort: None,
        account: None,
        yolo,
        yolo_until: until.map(Into::into),
    }
}

#[test]
fn layered_override_parses_and_old_format_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let r = aaf_runtime::dispatcher::Roster::from_json(
        r#"{"roles": {"c": {"driver": "acp", "acp_agent": "hermes", "model": "a",
            "room_overrides": {
              "3": {"model": "b", "mode": "acceptEdits"},
              "4": {"acp_agent": "claude", "yolo": true, "yolo_until": "2026-12-31"},
              "5": {"driver": "command", "command": ["/bin/cat"]}}}}}"#,
        dir.path(),
    )
    .unwrap();
    let get = |room| {
        r.spec_for_on(&Lane::new("c", Some(room)), "2026-10-13")
            .unwrap()
    };
    // 疊加：只改模型與模式，agent 繼承
    assert!(
        matches!(get(3), DriverSpec::Acp { acp_agent: Some(a), model: Some(m), mode: Some(md), .. } if a == "hermes" && m == "b" && md == "acceptEdits")
    );
    // 換 agent：模型不繼承（舊模型屬於別的 agent）；YOLO＝最寬模式
    assert!(
        matches!(get(4), DriverSpec::Acp { acp_agent: Some(a), model: None, mode: Some(md), .. } if a == "claude" && md == "@widest")
    );
    // 舊格式：整份取代
    assert!(matches!(get(5), DriverSpec::Command { .. }));
    // 沒覆寫的群：全域
    assert!(matches!(get(6), DriverSpec::Acp { model: Some(m), mode: None, .. } if m == "a"));
    // YOLO 期限含當日；過期回一般
    let o = layer(None, None, None, true, Some("2026-10-13"));
    assert!(o.yolo_on("2026-10-13") && !o.yolo_on("2026-10-14"));
    assert!(layer(None, None, None, true, None).yolo_on("2099-01-01"));
    // 壞日期拒絕
    assert!(aaf_runtime::dispatcher::Roster::from_json(
        r#"{"roles": {"c": {"driver": "manual", "room_overrides": {"1": {"yolo": true, "yolo_until": "soon"}}}}}"#,
        dir.path()
    )
    .is_err());
}

#[test]
fn widest_mode_matches_real_agents() {
    use aaf_runtime::catalog::{mode_switch, widest_mode, WIDEST};
    assert_eq!(
        widest_mode(&[
            "default",
            "acceptEdits",
            "plan",
            "auto",
            "bypassPermissions"
        ])
        .as_deref(),
        Some("bypassPermissions")
    );
    assert_eq!(
        widest_mode(&["read-only", "auto", "full-access"]).as_deref(),
        Some("full-access")
    );
    assert_eq!(
        widest_mode(&["default", "accept_edits", "dont_ask"]).as_deref(),
        Some("dont_ask")
    );
    assert_eq!(widest_mode(&["off"]), None);
    // claude 以 configOptions 回報：走 set_config_option
    let claude = serde_json::json!({"configOptions": [{"id": "mode", "category": "mode", "currentValue": "default",
        "options": [{"value": "default"}, {"value": "bypassPermissions"}]}]});
    let (m, p, id) = mode_switch(&claude, WIDEST).unwrap();
    assert_eq!(
        (m.as_str(), p["configId"].as_str(), id.as_str()),
        (
            "session/set_config_option",
            Some("mode"),
            "bypassPermissions"
        )
    );
    // hermes 以 modes 回報：走 set_mode
    let hermes = serde_json::json!({"modes": {"currentModeId": "default", "availableModes": [{"id": "default"}, {"id": "dont_ask"}]}});
    assert_eq!(mode_switch(&hermes, WIDEST).unwrap().0, "session/set_mode");
    // 已是目標、或 agent 沒有該模式：不切
    assert!(mode_switch(&hermes, "default").is_none());
    assert!(mode_switch(&hermes, "nope").is_none());
}

#[tokio::test]
async fn room_override_changes_only_that_lane_and_yolo_auto_allows() {
    let mut spec = fake_spec();
    if let DriverSpec::Acp { env, .. } = &mut spec {
        env.push(("FAKE_ALLOW_ALWAYS".into(), "1".into()));
    }
    let (inbox, bus, disp) = setup(&[("me", DriverSpec::Manual, false), ("coder", spec, true)]);
    // 兩個群各開一條 lane
    inbox.post("me", "coder", "WHOAMI", Some(1));
    inbox.post("me", "coder", "WHOAMI", Some(2));
    disp.once().await.unwrap();
    let sid = |room: i64| {
        inbox
            .all_to("me")
            .into_iter()
            .rfind(|m| m.room == Some(room) && m.body.starts_with("sess-"))
            .unwrap()
            .body
            .split(' ')
            .next()
            .unwrap()
            .to_string()
    };
    let (s1, s2) = (sid(1), sid(2));
    // 只改群 2：YOLO
    let mut r = (*disp.roster()).clone();
    r.roles
        .get_mut("coder")
        .unwrap()
        .room_overrides
        .insert(2, layer(None, None, None, true, None));
    let diff = disp.apply_roster(r).await.unwrap();
    assert!(diff.changed.is_empty(), "{diff:?}");
    assert_eq!(diff.lanes, vec![Lane::new("coder", Some(2))]);
    assert!(disp.draining().is_empty(), "role itself not drained");
    // 群 1 沿用同一個 session；群 2 是新 session、模式切到最寬
    inbox.post("me", "coder", "WHOAMI", Some(1));
    inbox.post("me", "coder", "MODE?", Some(2));
    disp.once().await.unwrap();
    assert_eq!(sid(1), s1);
    let last2 = inbox
        .all_to("me")
        .into_iter()
        .rfind(|m| m.room == Some(2))
        .unwrap()
        .body;
    assert_eq!(last2, "MODE: bypassPermissions");
    inbox.post("me", "coder", "WHOAMI", Some(2));
    disp.once().await.unwrap();
    assert_ne!(sid(2), s2, "new session after override");
    // YOLO：權限請求自動允許，優先 allow_always，永不選 reject
    inbox.post("me", "coder", "NEED_PERMISSION", Some(2));
    disp.once().await.unwrap();
    let r2 = inbox
        .all_to("me")
        .into_iter()
        .rfind(|m| m.room == Some(2))
        .unwrap()
        .body;
    assert_eq!(r2, "PERMISSION: always");
    let evs = bus.since(0, 2000).unwrap();
    assert!(evs
        .iter()
        .any(|e| e.kind == "runtime.permission.auto_approved"
            && e.payload["reason"]
                .as_str()
                .is_some_and(|r| r.contains("YOLO"))
            && e.payload["option"] == "always"));
    // 群 1 沒有 YOLO：不自動回應（等人）
    inbox.post("me", "coder", "NEED_PERMISSION", Some(1));
    disp.tick().await.unwrap();
    let mut asked = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        if bus
            .since(0, 4000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "runtime.permission.requested" && e.payload["lane"]["room"] == 1)
        {
            asked = true;
            break;
        }
    }
    assert!(asked);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!bus
        .since(0, 4000)
        .unwrap()
        .iter()
        .any(|e| e.kind == "runtime.permission.auto_approved" && e.payload["lane"]["room"] == 1));
    disp.cancel(&Lane::new("coder", Some(1))).await.unwrap();
}

#[tokio::test]
async fn explicit_mode_override_is_applied() {
    let (inbox, _bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    let mut r = (*disp.roster()).clone();
    r.roles
        .get_mut("coder")
        .unwrap()
        .room_overrides
        .insert(7, layer(None, None, Some("acceptEdits"), false, None));
    disp.apply_roster(r).await.unwrap();
    inbox.post("me", "coder", "MODE?", Some(7));
    inbox.post("me", "coder", "MODE?", Some(8));
    disp.once().await.unwrap();
    let body = |room| {
        inbox
            .all_to("me")
            .into_iter()
            .rfind(|m| m.room == Some(room))
            .unwrap()
            .body
    };
    assert_eq!(body(7), "MODE: acceptEdits");
    assert_eq!(body(8), "MODE: default");
}

#[tokio::test]
async fn empty_reply_marker_is_not_posted_but_acked() {
    let (inbox, bus, disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    inbox.post("me", "coder", "EMPTY_REPLY", Some(1));
    disp.once().await.unwrap();
    assert!(inbox.all_to("me").is_empty(), "(empty) must not be posted");
    assert!(
        inbox.lanes_with_unread().await.unwrap().is_empty(),
        "turn is acked so it is not re-run"
    );
    assert!(bus
        .since(0, 500)
        .unwrap()
        .iter()
        .any(|e| e.kind == "runtime.turn.empty_reply"));
    // 一般回覆照常貼出
    inbox.post("me", "coder", "hello", Some(1));
    disp.once().await.unwrap();
    assert_eq!(inbox.all_to("me").len(), 1);
}

#[tokio::test]
async fn stop_force_closes_lane_that_ignores_cancel() {
    let (inbox, bus, mut disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    disp.stop_grace = Duration::from_millis(200);
    let disp = Arc::new(disp);
    let mut ev = bus.subscribe();
    inbox.post("me", "coder", "HANG", Some(1));
    disp.tick().await.unwrap();
    let lane = Lane::new("coder", Some(1));
    for _ in 0..300 {
        if disp
            .lane_states()
            .await
            .iter()
            .any(|(l, s)| *l == lane && *s == AgentStatus::Working)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(disp.cancel(&lane).await.unwrap());
    // 寬限期內不動
    assert!(disp.reap_stuck().await.unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(disp.reap_stuck().await.unwrap(), vec![lane.clone()]);
    assert!(
        disp.lane_states().await.iter().all(|(l, _)| *l != lane),
        "lane closed"
    );
    let mut reason = None;
    while let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(500), ev.next()).await {
        if e.kind == "runtime.lane.force_closed" {
            reason = Some(e.payload["reason"].clone());
            break;
        }
    }
    assert_eq!(reason, Some(serde_json::json!("cancel_ignored")));
    // 之後的信照常派到新程序
    inbox.post("me", "coder", "after", Some(1));
    for _ in 0..300 {
        disp.tick().await.unwrap();
        if inbox.all_to("me").iter().any(|m| m.body == "ECHO: after") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("lane did not recover after force close");
}

#[tokio::test]
async fn watchdog_closes_lane_silent_too_long() {
    let (inbox, _bus, mut disp) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    disp.stall_kill = Duration::from_millis(300);
    let disp = Arc::new(disp);
    inbox.post("me", "coder", "HANG", Some(1));
    disp.tick().await.unwrap();
    let lane = Lane::new("coder", Some(1));
    for _ in 0..300 {
        if disp
            .lane_states()
            .await
            .iter()
            .any(|(l, s)| *l == lane && *s == AgentStatus::Working)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(disp.reap_stuck().await.unwrap().is_empty(), "not yet");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(disp.reap_stuck().await.unwrap(), vec![lane]);
}

// 回歸案例（兩個環境變數有預設值，CI 不必另設）
#[tokio::test]
async fn audit_background_permission_after_end_turn_returns_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let trigger = tmp.path().join("go");
    let journal = tmp.path().join("journal");
    let d = AcpDriver::new(
        vec![
            std::env::var("AUDIT_PYTHON").unwrap_or_else(|_| "python3".into()),
            std::env::var("AUDIT_LATE_AGENT").unwrap_or_else(|_| {
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/late_permission_agent.py"
                )
                .into()
            }),
            trigger.display().to_string(),
            journal.display().to_string(),
        ],
        tmp.path().to_path_buf(),
        vec![],
    );
    d.start().await.unwrap();
    let mut rx = d.events();
    let first = d.prompt("FIRST").await.unwrap();
    assert_eq!(first.text, "CONTROL_OK");
    assert_eq!(first.stop_reason, "end_turn");
    assert_eq!(d.status(), AgentStatus::Idle);
    std::fs::write(&trigger, "emit permission AFTER end_turn").unwrap();
    let ev = next_matching(&mut rx, |e| {
        matches!(e, DriverEvent::PermissionRequested { .. })
    })
    .await;
    let DriverEvent::PermissionRequested { request_id, .. } = ev else {
        unreachable!()
    };
    assert_eq!(d.status(), AgentStatus::Blocked);
    d.respond_permission(
        request_id,
        PermissionDecision::Selected {
            option_id: "allow".into(),
        },
    )
    .await
    .unwrap();
    let state = d.status();
    let second = tokio::time::timeout(Duration::from_secs(2), d.prompt("SECOND")).await;
    let summary = format!("state_after_background_approval={state:?}; second_prompt={second:?}");
    let prompt_count = std::fs::read_to_string(&journal).unwrap().lines().count();
    // Cleanup happens before the intentional RED assertion.
    d.stop().await.unwrap();
    println!(
        "{summary}; actual_agent_prompt_count={prompt_count}; stopped={:?}",
        d.status()
    );
    assert_eq!(
        state,
        AgentStatus::Idle,
        "background permission must not invent an active turn; {summary}"
    );
    assert!(
        matches!(second, Ok(Ok(_))),
        "next prompt must be accepted: {summary}"
    );
    assert_eq!(prompt_count, 2);
}

// 獨立審驗案例（移除 watchdog ack 兩項；fixture watchdog_agent.py）

// Independent audit cases.
async fn audit_state(d: &Dispatcher, lane: &Lane, wanted: AgentStatus) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if d.lane_states()
                .await
                .iter()
                .any(|(l, s)| l == lane && *s == wanted)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lane state reached");
}
async fn audit_settled(d: &Dispatcher, lane: &Lane) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while d.busy(lane) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn settled");
}
fn audit_acked(inbox: &MemInbox, id: i64) -> bool {
    inbox
        .msgs
        .lock()
        .unwrap()
        .iter()
        .find(|m| m.id == id)
        .unwrap()
        .acked
}
async fn audit_delivery_scenario(manual: bool) -> bool {
    let (inbox, bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    d.stop_grace = Duration::from_millis(120);
    d.stall_kill = if manual {
        Duration::ZERO
    } else {
        Duration::from_millis(220)
    };
    let lane = Lane::new("coder", Some(1));
    let other = Lane::new("coder", Some(2));
    let mut events = bus.subscribe();
    let original = inbox.post("me", "coder", "HANG original unfinished", Some(1));
    let batched = inbox.post("me", "coder", "original batched instruction", Some(1));
    d.tick().await.unwrap();
    audit_state(&d, &lane, AgentStatus::Working).await;
    let sid = inbox.load_session(&lane).await.unwrap().unwrap();
    let queued = inbox.post("me", "coder", "queued after original started", Some(1));
    let other_id = inbox.post("me", "coder", "SLOW other lane", Some(2));
    d.tick().await.unwrap();
    audit_state(&d, &other, AgentStatus::Working).await;
    let other_sid = inbox.load_session(&other).await.unwrap().unwrap();
    if manual {
        d.cancel(&lane).await.unwrap();
    }
    // Only target reaches stall threshold: other starts later for automatic case.
    if !manual {
        // Refresh the other lane by cooperative cancellation, then start another long turn.
        tokio::time::sleep(Duration::from_millis(140)).await;
        d.cancel(&other).await.unwrap();
        audit_settled(&d, &other).await;
        // Reap while Idle to clear the prior cancellation timestamp. A separate
        // counterexample tests the missing-idle-reap race without this control.
        assert!(d.reap_stuck().await.unwrap().is_empty());
        inbox.post("me", "coder", "SLOW other still working", Some(2));
        d.tick().await.unwrap();
        audit_state(&d, &other, AgentStatus::Working).await;
    }
    tokio::time::sleep(Duration::from_millis(140)).await;
    let closed = d.reap_stuck().await.unwrap();
    assert_eq!(closed, vec![lane.clone()]);
    audit_settled(&d, &lane).await;
    assert!(
        !audit_acked(&inbox, queued),
        "queued mail must remain unread before relaunch"
    );
    assert_eq!(
        inbox.load_session(&lane).await.unwrap().as_deref(),
        Some(sid.as_str())
    );
    assert_eq!(
        inbox.load_session(&other).await.unwrap().as_deref(),
        Some(other_sid.as_str())
    );
    audit_state(&d, &other, AgentStatus::Working).await;
    assert!(d.busy(&other));
    let original_acked = audit_acked(&inbox, original);
    let batched_acked = audit_acked(&inbox, batched);
    let original_reply_count = inbox
        .all_to("me")
        .iter()
        .filter(|m| m.room == Some(1) && m.from != "system")
        .count();
    d.tick().await.unwrap();
    audit_settled(&d, &lane).await;
    let replies = inbox.all_to("me");
    assert!(replies
        .iter()
        .any(|m| m.body == "ECHO: queued after original started"));
    assert!(audit_acked(&inbox, queued));
    assert_eq!(
        inbox.load_session(&lane).await.unwrap().as_deref(),
        Some(sid.as_str())
    );
    // WHOAMI establishes real session/load identity, not just retained database value.
    inbox.post("me", "coder", "WHOAMI", Some(1));
    d.tick().await.unwrap();
    audit_settled(&d, &lane).await;
    assert!(inbox
        .all_to("me")
        .iter()
        .any(|m| m.body == format!("{sid} turn 2")));
    d.cancel(&other).await.unwrap();
    audit_settled(&d, &other).await;
    inbox.post("me", "coder", "other recovered", Some(2));
    d.once().await.unwrap();
    assert!(inbox
        .all_to("me")
        .iter()
        .any(|m| m.body == "ECHO: other recovered"));
    let mut observed = Vec::new();
    while let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(30), events.next()).await {
        if [
            "runtime.turn.incomplete",
            "runtime.turn.failed",
            "runtime.lane.force_closed",
        ]
        .contains(&e.kind.as_str())
        {
            observed.push(serde_json::json!({"kind": e.kind, "payload": e.payload}));
        }
    }
    d.close(&lane, false).await.unwrap();
    d.close(&other, false).await.unwrap();
    println!("AUDIT manual={manual} original={original} original_acked={original_acked} batched={batched} batched_acked={batched_acked} original_reply_count={original_reply_count} queued={queued} queued_acked={} sid={sid} actual_resumed_whoami=true other_sid={other_sid} other_id={other_id} isolated=true events={}", audit_acked(&inbox, queued), serde_json::to_string(&observed).unwrap());
    assert_eq!(original_reply_count, 0);
    assert_eq!(original_acked, batched_acked);
    original_acked
}
#[tokio::test]
async fn audit_manual_fallback_queue_session_isolation() {
    assert!(audit_delivery_scenario(true).await);
}
#[tokio::test]
async fn audit_zero_disables_watchdog_but_not_manual_stop() {
    let (inbox, _, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    d.stall_kill = Duration::ZERO;
    d.stop_grace = Duration::from_millis(100);
    let lane = Lane::new("coder", Some(1));
    inbox.post("me", "coder", "HANG", Some(1));
    d.tick().await.unwrap();
    audit_state(&d, &lane, AgentStatus::Working).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(d.reap_stuck().await.unwrap().is_empty());
    assert!(d.busy(&lane));
    d.cancel(&lane).await.unwrap();
    tokio::time::sleep(Duration::from_millis(140)).await;
    assert_eq!(d.reap_stuck().await.unwrap(), vec![lane.clone()]);
    audit_settled(&d, &lane).await;
    println!("AUDIT stall_kill=0 kept Working; manual stop still closed lane");
}
fn audit_helper(mode: &str) -> DriverSpec {
    DriverSpec::Acp {
        command: vec![
            "python3".into(),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/watchdog_agent.py"
            )
            .into(),
            mode.into(),
        ],
        acp_agent: None,
        model: None,
        mode: None,
        cwd: None,
        env: vec![],
        effort: None,
        account: None,
        transport: None,
    }
}
#[tokio::test]
async fn audit_blocked_exempt_even_after_ignored_manual_stop() {
    let (inbox, _, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", audit_helper("blocked"), true),
    ]);
    d.stall_kill = Duration::from_millis(100);
    d.stop_grace = Duration::from_millis(100);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "pending", Some(1));
    d.tick().await.unwrap();
    audit_state(&d, &lane, AgentStatus::Blocked).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(d.reap_stuck().await.unwrap().is_empty());
    d.cancel(&lane).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let closed = d.reap_stuck().await.unwrap();
    assert!(closed.is_empty());
    audit_state(&d, &lane, AgentStatus::Blocked).await;
    assert!(!audit_acked(&inbox, id));
    println!("AUDIT Blocked watchdog exempt=true; explicit stop ignored >grace also exempt=true; original_unread=true");
    d.close(&lane, false).await.unwrap();
    audit_settled(&d, &lane).await;
}
#[tokio::test]
async fn audit_live_events_prevent_watchdog() {
    let (inbox, _, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", audit_helper("heartbeat"), true),
    ]);
    d.stall_kill = Duration::from_millis(180);
    let lane = Lane::new("coder", Some(1));
    inbox.post("me", "coder", "working", Some(1));
    d.tick().await.unwrap();
    audit_state(&d, &lane, AgentStatus::Working).await;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(d.reap_stuck().await.unwrap().is_empty());
    }
    println!("AUDIT heartbeat every 40ms kept Working for 480ms with watchdog=180ms");
    d.close(&lane, false).await.unwrap();
    audit_settled(&d, &lane).await;
}

#[tokio::test]
async fn audit_counterexample_old_cancel_must_not_kill_next_turn() {
    let (inbox, bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    d.stall_kill = Duration::ZERO;
    d.stop_grace = Duration::from_millis(120);
    let lane = Lane::new("coder", Some(1));
    let mut ev = bus.subscribe();
    inbox.post("me", "coder", "SLOW first turn", Some(1));
    d.tick().await.unwrap();
    audit_state(&d, &lane, AgentStatus::Working).await;
    d.cancel(&lane).await.unwrap();
    audit_settled(&d, &lane).await;
    let queued = inbox.post("me", "coder", "HANG new unrelated turn", Some(1));
    // Match production job ordering: tick -> park_idle -> reap_stuck.
    d.tick().await.unwrap();
    audit_state(&d, &lane, AgentStatus::Working).await;
    d.park_idle().await.unwrap();
    tokio::time::sleep(Duration::from_millis(160)).await;
    let closed = d.reap_stuck().await.unwrap();
    if !closed.is_empty() {
        audit_settled(&d, &lane).await;
    }
    let new_acked = audit_acked(&inbox, queued);
    let mut reasons = Vec::new();
    while let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(25), ev.next()).await {
        if e.kind == "runtime.lane.force_closed" {
            reasons.push(e.payload);
        }
    }
    d.close(&lane, false).await.unwrap();
    audit_settled(&d, &lane).await;
    println!("AUDIT prior turn cancelled cooperatively; no cancel on next turn; stall_kill=0; next_turn_closed={closed:?}; next_turn_acked={new_acked}; reasons={reasons:?}");
    assert!(closed.is_empty(), "Old cancel_at leaked into a new turn, force-closing and acking work the user never stopped");
}

// 看門狗語義（取代原本的 watchdog ack 兩項）：原批次不重派（重派會一再卡住），
// 但在群裡留一則可見的中斷通知，列出沒做完的訊息；之後排隊的訊息照常補送。
#[tokio::test]
async fn watchdog_interrupt_is_visible_and_queue_continues() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    d.stall_kill = Duration::from_millis(200);
    let lane = Lane::new("coder", Some(1));
    let a = inbox.post("me", "coder", "HANG original", Some(1));
    let b = inbox.post("me", "coder", "batched", Some(1));
    d.tick().await.unwrap();
    audit_state(&d, &lane, AgentStatus::Working).await;
    let q = inbox.post("me", "coder", "queued later", Some(1));
    tokio::time::sleep(Duration::from_millis(260)).await;
    assert_eq!(d.reap_stuck().await.unwrap(), vec![lane.clone()]);
    audit_settled(&d, &lane).await;
    let notice = inbox
        .all_to("me")
        .into_iter()
        .find(|m| m.body.contains("強制中斷"))
        .expect("visible notice");
    assert!(
        notice.body.contains(&format!("#{a}")) && notice.body.contains(&format!("#{b}")),
        "{}",
        notice.body
    );
    assert!(!audit_acked(&inbox, q));
    d.tick().await.unwrap();
    audit_settled(&d, &lane).await;
    assert!(inbox
        .all_to("me")
        .iter()
        .any(|m| m.body == "ECHO: queued later"));
    d.close(&lane, false).await.unwrap();
}

// ── 回合失敗不能無聲消失 ──────────────────────────────────────────

fn hermes_fake_spec() -> DriverSpec {
    DriverSpec::Acp {
        command: vec![env!("CARGO_BIN_EXE_aaf-fake-acp").into()],
        acp_agent: Some("hermes".into()),
        model: None,
        mode: None,
        cwd: None,
        env: vec![],
        effort: None,
        account: None,
        transport: None,
    }
}

fn notices(inbox: &MemInbox) -> Vec<Mail> {
    inbox
        .all_to("me")
        .into_iter()
        .filter(|m| m.from == "system")
        .collect()
}

fn role_posts(inbox: &MemInbox) -> Vec<Mail> {
    inbox
        .all_to("me")
        .into_iter()
        .filter(|m| m.from == "coder")
        .collect()
}

/// 跑回合直到該訊息被 ack 或次數用完；每輪等回合結束。
async fn drive(d: &Dispatcher, inbox: &MemInbox, lane: &Lane, id: i64, max: usize) -> usize {
    let mut started = 0;
    for _ in 0..max {
        if audit_acked(inbox, id) {
            break;
        }
        if !d.once().await.unwrap().is_empty() {
            started += 1;
        }
        audit_settled(d, lane).await;
    }
    started
}

fn zero_backoff(d: &mut Dispatcher, n: u32) {
    d.turn_retry = n;
    d.retry_backoff = vec![Duration::ZERO];
}

#[tokio::test]
async fn turn_failure_retry_recovers_without_notice() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 2);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "FAIL_ONCE hello", Some(1));
    let n = drive(&d, &inbox, &lane, id, 5).await;
    assert_eq!(n, 2, "one failure + one retry");
    assert!(audit_acked(&inbox, id));
    assert!(notices(&inbox).is_empty());
    assert!(role_posts(&inbox)
        .iter()
        .any(|m| m.body.starts_with("ECHO:")));
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_always_fail_notices_once_after_retries() {
    let (inbox, bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 2);
    let mut ev = bus.subscribe();
    let lane = Lane::new("coder", Some(1));
    let a = inbox.post("me", "coder", "ALWAYS_FAIL one", Some(1));
    let b = inbox.post("me", "coder", "two", Some(1));
    let n = drive(&d, &inbox, &lane, a, 10).await;
    assert_eq!(n, 3, "1 + AAF_TURN_RETRY(2) attempts");
    assert!(audit_acked(&inbox, a) && audit_acked(&inbox, b));
    let ns = notices(&inbox);
    assert_eq!(ns.len(), 1, "{ns:?}");
    assert!(ns[0].body.contains(&format!("#{a}")) && ns[0].body.contains(&format!("#{b}")));
    assert!(ns[0].body.contains("沒有完成"));
    // 之後不再重派
    assert!(d.once().await.unwrap().is_empty());
    let mut finals = 0;
    while let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(30), ev.next()).await {
        if e.kind == "runtime.turn.failed" && e.payload["final"] == true {
            finals += 1;
        }
    }
    assert_eq!(finals, 1);
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_retry_zero_notices_immediately() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 0);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "FAIL_ONCE x", Some(1));
    assert_eq!(drive(&d, &inbox, &lane, id, 5).await, 1);
    assert_eq!(notices(&inbox).len(), 1);
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_backoff_holds_the_lane() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    d.turn_retry = 2;
    d.retry_backoff = vec![Duration::from_secs(60)];
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "ALWAYS_FAIL", Some(1));
    d.once().await.unwrap();
    audit_settled(&d, &lane).await;
    assert!(d.once().await.unwrap().is_empty(), "backoff not respected");
    assert!(!audit_acked(&inbox, id));
    assert!(notices(&inbox).is_empty());
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_hermes_error_copy_is_not_a_role_reply() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", hermes_fake_spec(), true),
    ]);
    zero_backoff(&mut d, 1);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "ERRCOPY", Some(1));
    assert_eq!(drive(&d, &inbox, &lane, id, 5).await, 2);
    assert!(role_posts(&inbox).is_empty(), "{:?}", role_posts(&inbox));
    let ns = notices(&inbox);
    assert_eq!(ns.len(), 1);
    assert!(ns[0].body.contains("rejected this request as malformed"));
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_codex_connection_warning_is_retried_not_posted() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 2);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "CODEX_WARN_ONCE hello", Some(1));
    assert_eq!(
        drive(&d, &inbox, &lane, id, 5).await,
        2,
        "warning + one retry"
    );
    let posts = role_posts(&inbox);
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(
        posts[0].body.starts_with("ECHO:"),
        "only the real reply is posted"
    );
    assert!(notices(&inbox).is_empty());
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_codex_connection_warning_notices_after_retries() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 1);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "CODEX_WARN_ALWAYS", Some(1));
    assert_eq!(drive(&d, &inbox, &lane, id, 5).await, 2);
    assert!(role_posts(&inbox).is_empty(), "{:?}", role_posts(&inbox));
    assert_eq!(notices(&inbox).len(), 1);
    d.close(&lane, false).await.unwrap();
}

#[test]
fn codex_failure_copy_detection() {
    use aaf_runtime::dispatcher::codex_failure_copy;
    // 實際貼出的文字
    let w = "Warning: Falling back from WebSockets to HTTPS transport.";
    assert_eq!(
        codex_failure_copy(&format!(
            "{w} workspace routing discovery timed out\n\nworkspace routing discovery timed out"
        )),
        Some(true)
    );
    assert_eq!(
        codex_failure_copy(&format!("{w} request timed out\n\n(empty)")),
        Some(true)
    );
    assert_eq!(
        codex_failure_copy(&format!("{w} request timed out")),
        Some(true)
    );
    // 警告後面接著真正的回覆：照常貼出
    assert_eq!(
        codex_failure_copy(&format!(
            "{w} request timed out\n\n- 對應：alice #42\n- 大意：已完成。"
        )),
        None
    );
    assert_eq!(codex_failure_copy("好的，我來處理。"), None);
    assert_eq!(
        codex_failure_copy(
            "討論：Codex 會印出 Warning: Falling back from WebSockets to HTTPS transport."
        ),
        None
    );
}

#[tokio::test]
async fn turn_failure_error_copy_from_non_hermes_agent_is_posted() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 1);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "ERRCOPY", Some(1));
    drive(&d, &inbox, &lane, id, 3).await;
    assert_eq!(role_posts(&inbox).len(), 1);
    assert!(notices(&inbox).is_empty());
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_max_tokens_posts_partial_and_notice_without_retry() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 2);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "STOP_MAXTOKENS", Some(1));
    assert_eq!(drive(&d, &inbox, &lane, id, 5).await, 1);
    assert!(role_posts(&inbox)
        .iter()
        .any(|m| m.body == "PARTIAL answer"));
    let ns = notices(&inbox);
    assert_eq!(ns.len(), 1);
    assert!(ns[0].body.contains("不完整"));
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_refusal_notices_without_retry() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 2);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "STOP_REFUSAL", Some(1));
    assert_eq!(drive(&d, &inbox, &lane, id, 5).await, 1);
    assert!(notices(&inbox)[0].body.contains("拒絕"));
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_command_role_nonzero_exit_notices() {
    let spec = DriverSpec::Command {
        command: vec!["/bin/sh".into(), "-c".into(), "exit 3".into()],
        cwd: None,
    };
    let (inbox, _bus, mut d) = setup(&[("me", DriverSpec::Manual, false), ("coder", spec, true)]);
    zero_backoff(&mut d, 1);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "go", Some(1));
    assert_eq!(drive(&d, &inbox, &lane, id, 5).await, 2);
    let ns = notices(&inbox);
    assert_eq!(ns.len(), 1, "{ns:?}");
    assert!(ns[0].body.contains("exit"));
    d.close(&lane, false).await.unwrap();
}

#[tokio::test]
async fn turn_failure_cancel_posts_stop_notice_not_failure() {
    let (inbox, _bus, mut d) = setup(&[
        ("me", DriverSpec::Manual, false),
        ("coder", fake_spec(), true),
    ]);
    zero_backoff(&mut d, 2);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "SLOW", Some(1));
    d.tick().await.unwrap();
    audit_state(&d, &lane, AgentStatus::Working).await;
    d.cancel(&lane).await.unwrap();
    audit_settled(&d, &lane).await;
    assert!(audit_acked(&inbox, id));
    // 停止不算回合失敗：不重試、不發失敗公告，只發一則停止公告
    let ns = notices(&inbox);
    assert_eq!(ns.len(), 1, "{ns:?}");
    assert!(ns[0].body.contains("手動停止") && !ns[0].body.contains("沒有完成"));
    d.close(&lane, false).await.unwrap();
}

/// 回歸：判斷是否 Hermes 只看執行檔名；路徑含 `.hermes` 目錄的其他 agent 照常貼文。
#[tokio::test]
async fn turn_failure_hermes_detection_ignores_directory_names() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join(".hermes").join("bin");
    std::fs::create_dir_all(&nested).unwrap();
    let exe = nested.join("other-agent");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_aaf-fake-acp"), &exe).unwrap();
    let spec = DriverSpec::Acp {
        command: vec![exe.to_string_lossy().into()],
        acp_agent: None,
        model: None,
        mode: None,
        cwd: None,
        env: vec![],
        effort: None,
        account: None,
        transport: None,
    };
    let (inbox, _bus, mut d) = setup(&[("me", DriverSpec::Manual, false), ("coder", spec, true)]);
    zero_backoff(&mut d, 1);
    let lane = Lane::new("coder", Some(1));
    let id = inbox.post("me", "coder", "ERRCOPY", Some(1));
    drive(&d, &inbox, &lane, id, 3).await;
    assert_eq!(role_posts(&inbox).len(), 1);
    assert!(notices(&inbox).is_empty());
    d.close(&lane, false).await.unwrap();
}

#[test]
fn hermes_failure_copy_detection() {
    use aaf_runtime::dispatcher::hermes_failure_copy;
    assert_eq!(
        hermes_failure_copy(
            "GitHub Copilot rejected this request as malformed, so the model didn't answer."
        ),
        Some(true)
    );
    assert_eq!(
        hermes_failure_copy("Model 'x' isn't available on Foo. Pick another."),
        Some(false)
    );
    assert_eq!(hermes_failure_copy("好的，我來處理。"), None);
    // 實際貼進群裡的文字（Hermes content_policy_copy）
    assert_eq!(
        hermes_failure_copy(
            "⚠️ GitHub Copilot's safety filter refused this request, so the model didn't answer. Try rewording your message or removing sensitive attachments, or switch to another model with /model.\n\nProvider said: HTTP 422: This content was flagged for possible cybersecurity risk."
        ),
        Some(false)
    );
    let long = format!("{}rejected this request as malformed", "討論：".repeat(400));
    assert_eq!(
        hermes_failure_copy(&long),
        None,
        "long replies discussing the error are real replies"
    );
}
