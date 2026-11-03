//! `aaf doctor` 的整合測試：用建好的執行檔跑在暫存 instance。
use std::process::Command;

fn doctor(home: &std::path::Path, extra: &[&str]) -> (i32, serde_json::Value) {
    let o = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .args([
            "--home",
            home.to_str().unwrap(),
            "doctor",
            "--listen",
            "127.0.0.1:1",
            "--json",
        ])
        .args(extra)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&o.stdout))),
    )
}

fn level<'a>(v: &'a serde_json::Value, name: &str) -> &'a str {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .map(|c| c["level"].as_str().unwrap())
        .unwrap_or("absent")
}

#[test]
fn doctor_reports_and_never_writes_the_database() {
    let d = tempfile::tempdir().unwrap();
    let h = d.path();
    // 空 instance：只有警告，結束碼 0；不建立資料庫
    let (rc, v) = doctor(h, &[]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(
        (
            level(&v, "instance"),
            level(&v, "service"),
            level(&v, "database"),
            level(&v, "roles.json")
        ),
        ("ok", "warn", "warn", "warn")
    );
    assert!(
        !h.join("aaf.sqlite3").exists(),
        "doctor must not create the database"
    );

    // roles.json 壞掉 → fail、結束碼 1
    std::fs::create_dir_all(h.join("deploy")).unwrap();
    std::fs::write(h.join("deploy/roles.json"), "{\"roles\": {").unwrap();
    let (rc, v) = doctor(h, &[]);
    assert_eq!(rc, 1);
    assert_eq!(level(&v, "roles.json"), "fail");

    // 角色：未安裝的 agent、找不到的指令、可用的假 agent（探測成功）
    // 可用的指令：用 /bin/cat 當假 agent 只能測「找得到」；探測成功另由 runtime 的 catalog 測試涵蓋
    let fake = "/bin/cat".to_string();
    let roles = serde_json::json!({"roles": {
        "a": {"driver": "acp", "acp_agent": "codex"},
        "b": {"driver": "command", "command": ["definitely-not-a-binary"]},
        "c": {"driver": "command", "command": [fake]},
    }});
    std::fs::write(h.join("deploy/roles.json"), roles.to_string()).unwrap();
    let (rc, v) = doctor(h, &["--probe"]);
    assert_eq!(rc, 1);
    assert_eq!(
        (
            level(&v, "role a"),
            level(&v, "role b"),
            level(&v, "role c")
        ),
        ("fail", "fail", "ok"),
        "{v}"
    );

    // 已有資料庫（由 Db::open 建立）：唯讀檢查、權限警告；doctor 前後檔案內容不變
    drop(aaf_core::storage::Db::open(&h.join("aaf.sqlite3")).unwrap());
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        h.join("aaf.sqlite3"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let before = std::fs::read(h.join("aaf.sqlite3")).unwrap();
    let (_, v) = doctor(h, &[]);
    assert_eq!(level(&v, "database"), "ok", "{v}");
    assert_eq!(level(&v, "file permissions"), "warn");
    assert_eq!(
        std::fs::read(h.join("aaf.sqlite3")).unwrap(),
        before,
        "database untouched"
    );
    // 不可讀的資料庫 → fail
    std::fs::write(
        h.join("aaf.sqlite3"),
        b"not a database at all, just text....................",
    )
    .unwrap();
    let (rc, v) = doctor(h, &[]);
    assert_eq!(rc, 1);
    assert_eq!(level(&v, "database"), "fail", "{v}");
}
