//! `aaf doctor`：唯讀健康檢查（源自先前的 Python 版）。
//! 服務停著也能跑；不印任何機密值。結果：ok／warn／fail；有 fail 時結束碼 1。

use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
    /// 怎麼修（fail／warn 時）
    pub fix: Option<String>,
}

fn ok(name: impl Into<String>, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        level: Level::Ok,
        detail: detail.into(),
        fix: None,
    }
}
fn warn(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        level: Level::Warn,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}
fn fail(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        level: Level::Fail,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

pub struct Opts {
    pub listen: String,
    /// 探測每個角色的 agent（啟動 ACP 程序做 initialize＋session/new；不送提示、不花 token）
    pub probe: bool,
}

pub async fn run(home: &Path, o: &Opts) -> Vec<Check> {
    let mut out = Vec::new();
    // 1. instance 目錄可寫
    let _ = std::fs::create_dir_all(home.join("var"));
    let probe_file = home.join("var").join(".doctor-write-test");
    match std::fs::write(&probe_file, "ok").and_then(|_| std::fs::remove_file(&probe_file)) {
        Ok(()) => out.push(ok("instance", format!("{} writable", home.display()))),
        Err(e) => out.push(fail(
            "instance",
            format!("{}: {e}", home.display()),
            "check the path and its permissions",
        )),
    }
    // 2. 服務：鎖檔與埠
    let lock = home.join("var").join("aaf.lock");
    let running = lock_held(&lock);
    let port_open = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(&o.listen),
    )
    .await
    .is_ok_and(|r| r.is_ok());
    match (running, port_open) {
        (Some(pid), true) => out.push(ok(
            "service",
            format!("running (pid {pid}) on http://{}/app/", o.listen),
        )),
        (Some(pid), false) => out.push(warn(
            "service",
            format!("running (pid {pid}) but nothing answers on {}", o.listen),
            "it may listen on another address; check --listen",
        )),
        (None, true) => out.push(warn(
            "service",
            format!("{} is in use by another program", o.listen),
            "stop that program or choose another --listen",
        )),
        (None, false) => out.push(warn(
            "service",
            "not running",
            format!("aaf --home {} start --listen {}", home.display(), o.listen),
        )),
    }
    // 3. 資料庫
    let dbp = home.join("aaf.sqlite3");
    if !dbp.exists() {
        out.push(warn(
            "database",
            "not created yet",
            "start the service once (aaf up)",
        ));
    } else {
        // 唯讀開啟：doctor 不建表、不改 journal 模式
        let r = rusqlite::Connection::open_with_flags(
            &dbp,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .and_then(|c| {
            c.busy_timeout(Duration::from_secs(5))?;
            let integ: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
            let owners: i64 = c
                .query_row(
                    "SELECT count(*) FROM auth_users WHERE rank = 'owner'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(-1);
            let size: i64 = c
                .query_row(
                    "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            Ok((integ, owners, size))
        });
        match r {
            Ok((integ, _, _)) if integ != "ok" => out.push(fail(
                "database",
                format!("integrity: {integ}"),
                "stop the service and restore from a backup",
            )),
            Ok((_, owners, size)) => {
                out.push(ok(
                    "database",
                    format!("ok · {:.1} MB", size as f64 / 1048576.0),
                ));
                if owners == 0 {
                    out.push(warn(
                        "owner account",
                        "none yet",
                        "open the web page and create the owner account",
                    ));
                } else if owners > 0 {
                    out.push(ok("owner account", format!("{owners}")));
                }
            }
            Err(e) => out.push(fail(
                "database",
                format!("cannot read: {e}"),
                "restore aaf.sqlite3 from a backup",
            )),
        }
    }
    // 4. 敏感檔權限
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut loose = vec![];
        for f in [
            ".aaf.env",
            "aaf.sqlite3",
            "aaf.sqlite3-wal",
            "aaf.sqlite3-shm",
        ] {
            if let Ok(m) = std::fs::metadata(home.join(f)) {
                if m.permissions().mode() & 0o077 != 0 {
                    loose.push(format!("{f} ({:o})", m.permissions().mode() & 0o777));
                }
            }
        }
        if loose.is_empty() {
            out.push(ok("file permissions", "private"));
        } else {
            out.push(warn(
                "file permissions",
                format!("readable by others: {}", loose.join(", ")),
                "chmod 600 these files (aaf up does this at start)",
            ));
        }
    }
    // 5. roles.json
    let roster = match aaf_runtime::dispatcher::Roster::load(home) {
        Ok(r) => {
            let src = home.join("deploy").join("roles.json");
            out.push(if src.exists() {
                ok("roles.json", format!("{} role(s)", r.roles.len()))
            } else {
                warn(
                    "roles.json",
                    "missing (no agents)",
                    "add an agent on the Agents page, or write deploy/roles.json",
                )
            });
            Some(r)
        }
        Err(e) => {
            out.push(fail(
                "roles.json",
                e.to_string(),
                "fix deploy/roles.json (the running service keeps the last good version)",
            ));
            None
        }
    };
    // 6. 每個角色：指令可解析、登入、（可選）探測
    if let Some(r) = &roster {
        let mut names: Vec<&String> = r.roles.keys().collect();
        names.sort();
        let mut auth_seen = std::collections::HashMap::new();
        for name in names {
            let spec = &r.roles[name];
            let label = format!("role {name}");
            match &spec.driver {
                aaf_runtime::DriverSpec::Manual => out.push(ok(label, "manual")),
                aaf_runtime::DriverSpec::Command { command, .. } => {
                    out.push(cmd_check(label, command))
                }
                aaf_runtime::DriverSpec::Acp {
                    command,
                    acp_agent,
                    model,
                    ..
                } => {
                    let cmd = if command.is_empty() {
                        match acp_agent
                            .as_deref()
                            .and_then(|a| aaf_runtime::catalog::resolve_cmd(a, home))
                        {
                            Some(c) => c,
                            None => {
                                out.push(fail(
                                    &label,
                                    format!(
                                        "agent {} is not installed",
                                        acp_agent.as_deref().unwrap_or("?")
                                    ),
                                    "install it on the Agents page",
                                ));
                                continue;
                            }
                        }
                    } else {
                        command.clone()
                    };
                    let c = cmd_check(label.clone(), &cmd);
                    if c.level != Level::Ok {
                        out.push(c);
                        continue;
                    }
                    let what = format!(
                        "{}{}{}",
                        acp_agent.as_deref().unwrap_or("acp"),
                        model
                            .as_deref()
                            .map(|m| format!(" · {m}"))
                            .unwrap_or_default(),
                        if spec.driver.headless().is_some() {
                            " · headless"
                        } else {
                            ""
                        }
                    );
                    // headless 不用轉接器跑回合，但要 agent 自己的 CLI；模型清單仍用轉接器探測（不花 token）
                    if let Some(cli) = spec.driver.headless() {
                        if aaf_runtime::catalog::which(cli.program()).is_none() {
                            out.push(fail(
                                &label,
                                format!("{what} · `{}` CLI not on PATH", cli.program()),
                                "install the agent's CLI, or switch the role back to ACP",
                            ));
                            continue;
                        }
                    }
                    if o.probe {
                        let p = aaf_runtime::catalog::probe(
                            &cmd,
                            &spec.workdir,
                            Duration::from_secs(60),
                        )
                        .await;
                        if p["ok"] == true {
                            let model_ok = model.as_deref().is_none_or(|m| {
                                let ids: Vec<&str> = p["models"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|x| x["id"].as_str())
                                    .collect();
                                ids.is_empty()
                                    || ids.iter().any(|i| {
                                        *i == m
                                            || i.ends_with(&format!(":{m}"))
                                            || i.ends_with(&format!("/{m}"))
                                    })
                            });
                            out.push(if model_ok {
                                ok(&label, format!("{what} · connects"))
                            } else {
                                fail(
                                    &label,
                                    format!("{what} · model not offered by the agent"),
                                    "pick another model on the Agents page",
                                )
                            });
                        } else {
                            out.push(fail(
                                &label,
                                format!(
                                    "{what} · {}",
                                    p["error"].as_str().unwrap_or("probe failed")
                                ),
                                "run the agent's own login, or check its install",
                            ));
                        }
                    } else {
                        out.push(ok(&label, format!("{what} · {}", cmd[0])));
                    }
                    if let Some(a) = acp_agent
                        .as_deref()
                        .and_then(|a| aaf_runtime::catalog::get(a, home))
                    {
                        if !auth_seen.contains_key(&a.id) {
                            let (st, note) = aaf_runtime::catalog::auth_check(&a).await;
                            auth_seen.insert(a.id.clone(), ());
                            match st {
                                Some(true) => out.push(ok(format!("login {}", a.id), note)),
                                Some(false) => {
                                    out.push(fail(format!("login {}", a.id), note, a.needs.clone()))
                                }
                                None => {}
                            }
                        }
                    }
                }
            }
        }
    }
    // 7. hooks
    let hooks_dir = std::env::var_os("AAF_HOOKS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join("hooks"));
    let rules = aaf_runtime::hooks::Hooks::new(&hooks_dir, home).rules();
    if !rules.is_empty() {
        out.push(ok(
            "hooks",
            format!("{} rule(s) in {}", rules.len(), hooks_dir.display()),
        ));
    }
    let hl = home.join("var").join("hooks.log");
    if let Ok(t) = std::fs::read_to_string(&hl) {
        let recent_err = t
            .lines()
            .rev()
            .take(200)
            .filter(|l| {
                l.contains("timed out") || l.contains("exited with") || l.contains("cannot start")
            })
            .count();
        if recent_err > 0 {
            out.push(warn(
                "hook errors",
                format!("{recent_err} recent failure(s) (allowed through)"),
                format!("see {}", hl.display()),
            ));
        }
    }
    // 8. 磁碟空間
    if let Some(free) = free_bytes(home) {
        let gb = free as f64 / 1e9;
        out.push(if gb < 1.0 {
            warn(
                "disk",
                format!("{gb:.1} GB free"),
                "free some space; uploads and transcripts need room",
            )
        } else {
            ok("disk", format!("{gb:.0} GB free"))
        });
    }
    out
}

fn cmd_check(label: String, cmd: &[String]) -> Check {
    let Some(exe) = cmd.first() else {
        return fail(label, "empty command", "fix deploy/roles.json");
    };
    let found = if exe.contains('/') {
        std::path::Path::new(exe).is_file().then(|| exe.clone())
    } else {
        aaf_runtime::catalog::which(exe).map(|p| p.to_string_lossy().to_string())
    };
    match found {
        Some(p) => ok(label, p),
        None => fail(
            label,
            format!("{exe} not found"),
            "install it, or fix the command in deploy/roles.json",
        ),
    }
}

/// 鎖檔被持有＝服務在跑；回 pid。
pub(crate) fn lock_held(path: &Path) -> Option<String> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .ok()?;
    match f.try_lock() {
        Ok(()) => {
            let _ = f.unlock();
            None
        }
        Err(_) => Some(
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .trim()
                .to_string(),
        ),
    }
}

fn free_bytes(p: &Path) -> Option<u64> {
    let s = nix::sys::statvfs::statvfs(p).ok()?;
    Some(s.blocks_available() as u64 * s.fragment_size() as u64)
}

pub fn print(checks: &[Check], as_json: bool) -> bool {
    let failed = checks.iter().any(|c| c.level == Level::Fail);
    if as_json {
        let v: Vec<Value> = checks
            .iter()
            .map(|c| json!({"name": c.name, "level": match c.level { Level::Ok => "ok", Level::Warn => "warn", Level::Fail => "fail" }, "detail": c.detail, "fix": c.fix}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok": !failed, "checks": v})).unwrap()
        );
        return !failed;
    }
    let w = checks
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    for c in checks {
        let mark = match c.level {
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Fail => "✗",
        };
        println!("{mark} {:<w$}  {}", c.name, c.detail);
        if let Some(f) = &c.fix {
            println!("  {:<w$}  → {f}", "");
        }
    }
    let (n_w, n_f) = (
        checks.iter().filter(|c| c.level == Level::Warn).count(),
        checks.iter().filter(|c| c.level == Level::Fail).count(),
    );
    println!(
        "\n{} checks · {n_w} warning(s) · {n_f} failure(s)",
        checks.len()
    );
    !failed
}
