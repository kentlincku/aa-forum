//! 轉接器安裝工作：npm 在 `<instance>/vendor/acp` 執行，同時只能一個；輸出寫檔供輪詢。
//!
//! 兩種工作：
//! - install：直接 `npm install --save-exact <pkg>`（第一次安裝、自訂 agent）。
//! - upgrade：先裝進暫存目錄，探測＋送一句「只回 OK」通過
//!   才裝進 vendor（先移除改名前的舊套件）；失敗保留舊版。成功後呼叫 `after`（重開用到的 lane）。

use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    /// install／upgrade
    pub kind: String,
    pub agent: String,
    pub npm: String,
    pub cmd: Vec<String>,
    pub state: String,
    pub exit: Option<i32>,
    pub started: u64,
    pub finished: Option<u64>,
    #[serde(skip)]
    pub log: PathBuf,
}

#[derive(Clone, Default)]
pub struct Jobs(Arc<Mutex<HashMap<String, Job>>>);

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 自訂 agent：安裝成功後加進 custom.json。
#[derive(Debug, Clone)]
pub struct CustomPlan {
    pub id: String,
    pub label: String,
    pub acp: String,
}

/// 升級成功後要做的事（例如重開 lane）；回一行寫進輸出。
pub type After = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = String> + Send>> + Send>;

pub struct Upgrade {
    pub agent: String,
    /// `name@version`
    pub pkg: String,
    /// ACP 執行檔名（node_modules/.bin 下）
    pub exe: String,
    /// 改名前的舊套件（裝了就移除）
    pub legacy: Option<String>,
    pub check_timeout: Duration,
    /// 實測時叫轉接器用的本體（engine_env）
    pub engine_env: Vec<(String, String)>,
    /// 本體版本（寫相容紀錄用）
    pub engine_version: Option<String>,
    pub after: Option<After>,
}

fn npm_args(npm_bin: &Path, verb: &str, pkg: &str) -> Vec<String> {
    let mut v = vec![npm_bin.to_string_lossy().to_string(), verb.into()];
    v.extend(["--no-audit".into(), "--no-fund".into()]);
    if verb == "install" {
        v.push("--save-exact".into());
    }
    v.push(pkg.into());
    v
}

fn ensure_pkg_json(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    if !dir.join("package.json").exists() {
        std::fs::write(
            dir.join("package.json"),
            "{\n \"name\": \"aaf-acp-adapters\",\n \"private\": true,\n \"dependencies\": {}\n}\n",
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl Jobs {
    pub fn get(&self, id: &str) -> Option<Job> {
        self.0.lock().unwrap().get(id).cloned()
    }

    /// 登記一個新工作（同時只能一個）。
    fn begin(
        &self,
        instance: &Path,
        kind: &str,
        agent: &str,
        pkg: &str,
        cmd: Vec<String>,
    ) -> Result<Job, String> {
        if !aaf_runtime::catalog::valid_npm(pkg) {
            return Err("invalid npm package name".into());
        }
        let logs = instance.join("var").join("acp-install");
        std::fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
        let job = {
            let mut jobs = self.0.lock().unwrap();
            if jobs.values().any(|j| j.state == "running") {
                return Err("another install is running; please wait".into());
            }
            let id = format!("{:x}{:x}", now(), jobs.len() + 1);
            let job = Job {
                id: id.clone(),
                kind: kind.into(),
                agent: agent.into(),
                npm: pkg.into(),
                cmd,
                state: "running".into(),
                exit: None,
                started: now(),
                finished: None,
                log: logs.join(format!("{id}.log")),
            };
            jobs.insert(id, job.clone());
            job
        };
        std::fs::write(&job.log, "").map_err(|e| e.to_string())?;
        Ok(job)
    }

    fn finish(&self, id: &str, ok: bool, code: i32) {
        if let Some(j) = self.0.lock().unwrap().get_mut(id) {
            j.state = if ok { "done" } else { "failed" }.into();
            j.exit = Some(code);
            j.finished = Some(now());
        }
    }

    /// 跑一個子程序（不經 shell），輸出接到工作 log；回結束碼（-1＝起不來／逾時）。
    async fn run(&self, id: &str, cmd: &[String], cwd: &Path, timeout: Duration) -> i32 {
        self.append(
            id,
            &format!("$ {}\n(in {})\n", cmd.join(" "), cwd.display()),
        );
        let Some(j) = self.get(id) else { return -1 };
        let Ok(f) = std::fs::OpenOptions::new().append(true).open(&j.log) else {
            return -1;
        };
        let err = f.try_clone().ok();
        let mut c = tokio::process::Command::new(&cmd[0]);
        c.args(&cmd[1..])
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(f)
            .kill_on_drop(true);
        if let Some(e) = err {
            c.stderr(e);
        }
        match c.spawn() {
            Err(e) => {
                self.append(id, &format!("\ncannot start npm: {e}\n"));
                -1
            }
            Ok(mut ch) => match tokio::time::timeout(timeout, ch.wait()).await {
                Ok(Ok(s)) => s.code().unwrap_or(-1),
                Ok(Err(e)) => {
                    self.append(id, &format!("\n{e}\n"));
                    -1
                }
                Err(_) => {
                    let _ = ch.kill().await;
                    self.append(id, &format!("\ntimed out after {}s\n", timeout.as_secs()));
                    -1
                }
            },
        }
    }

    /// 開始安裝。`npm_bin`：測試時可換成假 npm。
    pub fn start(
        &self,
        instance: &Path,
        npm_bin: PathBuf,
        agent: &str,
        pkg: &str,
        custom: Option<CustomPlan>,
        timeout: Duration,
    ) -> Result<Job, String> {
        let vendor = aaf_runtime::catalog::vendor_dir(instance);
        let cmd = npm_args(&npm_bin, "install", pkg);
        let job = self.begin(instance, "install", agent, pkg, cmd.clone())?;
        ensure_pkg_json(&vendor)?;
        let me = self.clone();
        let (instance, jid) = (instance.to_path_buf(), job.id.clone());
        tokio::spawn(async move {
            let code = me.run(&jid, &cmd, &vendor, timeout).await;
            let agent = me.get(&jid).map(|j| j.agent).unwrap_or_default();
            if code == 0 {
                if let Some(p) = &custom {
                    if let Err(e) = add_custom(
                        &instance,
                        p,
                        me.get(&jid).map(|j| j.npm).unwrap_or_default(),
                    ) {
                        me.append(&jid, &format!("\nsaving custom agent failed: {e}\n"));
                    }
                }
            }
            let resolved = aaf_runtime::catalog::resolve_cmd(&agent, &instance);
            let ok = code == 0 && resolved.is_some();
            me.append(
                &jid,
                &format!(
                    "\nexit {code}; {}\n",
                    match &resolved {
                        Some(c) if ok => format!("ACP command ready: {}", c.join(" ")),
                        _ => "ACP command still not found after install".into(),
                    }
                ),
            );
            me.finish(&jid, ok, code);
        });
        Ok(job)
    }

    /// 升級／換版本：暫存目錄先裝＋實測，通過才動 vendor。
    pub fn start_upgrade(
        &self,
        instance: &Path,
        npm_bin: PathBuf,
        up: Upgrade,
        timeout: Duration,
    ) -> Result<Job, String> {
        if up.exe.is_empty()
            || !up
                .exe
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
        {
            return Err("invalid adapter executable name".into());
        }
        if let Some(l) = &up.legacy {
            if !aaf_runtime::catalog::valid_npm(l) {
                return Err("invalid npm package name".into());
            }
        }
        let vendor = aaf_runtime::catalog::vendor_dir(instance);
        let cmd = npm_args(&npm_bin, "install", &up.pkg);
        let job = self.begin(instance, "upgrade", &up.agent, &up.pkg, cmd.clone())?;
        let stage = instance.join("var").join("acp-stage").join(&job.id);
        let work = instance.join("work").join("_probe");
        let me = self.clone();
        let (instance, jid) = (instance.to_path_buf(), job.id.clone());
        tokio::spawn(async move {
            let say = |t: String| me.append(&jid, &t);
            let mut code = -1;
            let ok = 'run: {
                say("1/3 install into a staging folder\n".into());
                if let Err(e) = ensure_pkg_json(&stage).and(ensure_pkg_json(&vendor)) {
                    say(format!("cannot prepare folders: {e}\n"));
                    break 'run false;
                }
                let _ = std::fs::create_dir_all(&work);
                code = me.run(&jid, &cmd, &stage, timeout).await;
                if code != 0 {
                    say(format!(
                        "\nexit {code}; staging install failed — nothing changed\n"
                    ));
                    break 'run false;
                }
                say(
                    "\n2/3 test the new version (start it, send one \"Reply with exactly: OK\")\n"
                        .into(),
                );
                let bin = stage.join("node_modules").join(".bin").join(&up.exe);
                let check = aaf_runtime::catalog::check_model_env(
                    &[bin.to_string_lossy().to_string()],
                    &up.engine_env,
                    &work,
                    "",
                    up.check_timeout,
                )
                .await;
                let new_ver = aaf_runtime::catalog::split_npm(&up.pkg).1.map(String::from);
                crate::compat::record(
                    &instance,
                    &up.agent,
                    new_ver.as_deref(),
                    up.engine_version.as_deref(),
                    &check,
                    "adapter upgrade test",
                );
                if check["ok"] != true {
                    say(format!(
                        "test failed: {} — nothing changed, the current version stays\n",
                        check["error"].as_str().unwrap_or("?")
                    ));
                    break 'run false;
                }
                say(format!(
                    "test ok: model {} replied {:?}\n\n3/3 install into the vendor folder\n",
                    check["model"].as_str().unwrap_or("?"),
                    check["reply"].as_str().unwrap_or("")
                ));
                // 舊套件先移除：新舊 bin 同名時，之後才移除會把新裝好的 .bin 連結一起刪掉
                let mut removed = None;
                if let Some(l) = &up.legacy {
                    if let Some(v) = aaf_runtime::catalog::installed_version(&instance, l) {
                        let un = npm_args(Path::new(&cmd[0]), "uninstall", l);
                        if me.run(&jid, &un, &vendor, timeout).await == 0 {
                            removed = Some(format!("{l}@{v}"));
                        } else {
                            say(format!("\nremoving the old package {l} failed\n"));
                        }
                    }
                }
                code = me.run(&jid, &cmd, &vendor, timeout).await;
                if code != 0 {
                    say(format!("\nexit {code}; vendor install failed\n"));
                    if let Some(old) = removed {
                        say(format!("putting back {old}\n"));
                        let back = npm_args(Path::new(&cmd[0]), "install", &old);
                        me.run(&jid, &back, &vendor, timeout).await;
                    }
                    break 'run false;
                }
                match aaf_runtime::catalog::resolve_cmd(&up.agent, &instance) {
                    Some(c) => say(format!("\nACP command ready: {}\n", c.join(" "))),
                    None => {
                        say("\nACP command not found after install\n".into());
                        break 'run false;
                    }
                }
                if let Some(f) = up.after {
                    say(format!("{}\n", f().await));
                }
                true
            };
            let _ = std::fs::remove_dir_all(&stage);
            me.finish(&jid, ok, code);
        });
        Ok(job)
    }

    /// 其他模組（本體升級）用：登記工作、跑指令、寫輸出、結束。
    pub(crate) fn begin_kind(
        &self,
        instance: &Path,
        kind: &str,
        agent: &str,
        label: &str,
    ) -> Result<Job, String> {
        let logs = instance.join("var").join("acp-install");
        std::fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
        let mut jobs = self.0.lock().unwrap();
        if jobs.values().any(|j| j.state == "running") {
            return Err("another install is running; please wait".into());
        }
        let id = format!("{:x}{:x}", now(), jobs.len() + 1);
        let job = Job {
            id: id.clone(),
            kind: kind.into(),
            agent: agent.into(),
            npm: label.into(),
            cmd: vec![],
            state: "running".into(),
            exit: None,
            started: now(),
            finished: None,
            log: logs.join(format!("{id}.log")),
        };
        std::fs::write(&job.log, "").map_err(|e| e.to_string())?;
        jobs.insert(id, job.clone());
        Ok(job)
    }

    pub(crate) async fn run_cmd(
        &self,
        id: &str,
        cmd: &[String],
        cwd: &Path,
        timeout: Duration,
    ) -> i32 {
        self.run(id, cmd, cwd, timeout).await
    }

    pub(crate) fn say(&self, id: &str, text: &str) {
        self.append(id, text)
    }

    pub(crate) fn finish_pub(&self, id: &str, ok: bool, code: i32) {
        self.finish(id, ok, code)
    }

    fn append(&self, id: &str, text: &str) {
        use std::io::Write;
        if let Some(j) = self.get(id) {
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(j.log) {
                let _ = f.write_all(text.as_bytes());
            }
        }
    }

    /// 輸出（從 offset 起）。
    pub fn output(&self, id: &str, offset: usize) -> Option<(Job, String, usize)> {
        let j = self.get(id)?;
        let text = std::fs::read(&j.log).unwrap_or_default();
        let off = offset.min(text.len());
        let s = String::from_utf8_lossy(&text[off..]).to_string();
        Some((j, s, text.len()))
    }
}

fn add_custom(instance: &Path, p: &CustomPlan, npm: String) -> Result<(), String> {
    let f = aaf_runtime::catalog::custom_file(instance);
    let mut cur: Vec<serde_json::Value> = std::fs::read_to_string(&f)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    cur.retain(|e| e["id"] != p.id.as_str());
    cur.push(serde_json::json!({"id": p.id, "label": p.label, "acp": [p.acp], "source": "vendor", "npm": npm, "needs": "custom ACP adapter (check the agent itself and its login)"}));
    std::fs::write(&f, serde_json::to_string_pretty(&cur).unwrap()).map_err(|e| e.to_string())
}

/// 升級自訂 agent 後，更新 custom.json 記錄的 npm 版本。
pub fn set_custom_npm(instance: &Path, id: &str, npm: &str) -> Result<(), String> {
    let f = aaf_runtime::catalog::custom_file(instance);
    let Ok(t) = std::fs::read_to_string(&f) else {
        return Ok(());
    };
    let mut cur: Vec<serde_json::Value> = serde_json::from_str(&t).unwrap_or_default();
    for e in &mut cur {
        if e["id"] == id {
            e["npm"] = serde_json::json!(npm);
        }
    }
    std::fs::write(&f, serde_json::to_string_pretty(&cur).unwrap()).map_err(|e| e.to_string())
}

/// `npm view <name> version deprecated --json`：回 (latest, deprecated)。
pub async fn npm_latest(npm_bin: &Path, name: &str) -> Result<(String, Option<String>), String> {
    if !aaf_runtime::catalog::valid_npm(name) {
        return Err("invalid npm package name".into());
    }
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(npm_bin)
            .args(["view", name, "version", "deprecated", "--json"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "npm view timed out".to_string())?
    .map_err(|e| format!("cannot run npm: {e}"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "npm view {name}: {}",
            e.trim().lines().last().unwrap_or("failed")
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("npm view: {e}"))?;
    // 只有 version 時 npm 回字串；有 deprecated 時回物件
    let (ver, dep) = match &v {
        serde_json::Value::String(s) => (Some(s.clone()), None),
        o => (
            o["version"].as_str().map(String::from),
            o["deprecated"].as_str().map(String::from),
        ),
    };
    Ok((ver.ok_or("npm view: no version")?, dep))
}

/// 查 npm 套件的 bin，回安裝計畫（不安裝）。
pub async fn plan_custom(npm_bin: &Path, spec: &str) -> Result<serde_json::Value, String> {
    let spec = spec.trim();
    if !aaf_runtime::catalog::valid_npm(spec) {
        return Err("invalid npm package name (e.g. some-acp or @scope/pkg@1.2.3)".into());
    }
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(npm_bin)
            .args(["view", spec, "name", "version", "bin", "--json"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "npm view timed out".to_string())?
    .map_err(|e| format!("cannot run npm: {e}"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "npm cannot find {spec}: {}",
            e.trim()
                .chars()
                .rev()
                .take(200)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ));
    }
    let mut info: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("npm view: {e}"))?;
    if let Some(last) = info.as_array().and_then(|a| a.last()).cloned() {
        info = last;
    }
    let name = info["name"]
        .as_str()
        .ok_or("npm view: no name")?
        .to_string();
    let version = info["version"]
        .as_str()
        .ok_or("npm view: no version")?
        .to_string();
    let bins: Vec<String> = match &info["bin"] {
        serde_json::Value::String(_) => vec![name.rsplit('/').next().unwrap_or(&name).to_string()],
        serde_json::Value::Object(m) => m.keys().cloned().collect(),
        _ => vec![],
    };
    if bins.is_empty() {
        return Err(format!(
            "{spec} has no executable (bin); it is not an ACP adapter"
        ));
    }
    let id: String = name
        .rsplit('/')
        .next()
        .unwrap_or(&name)
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(24)
        .collect();
    let id = if id.is_empty() { "custom".into() } else { id };
    let pick = bins
        .iter()
        .find(|b| b.contains("acp"))
        .unwrap_or(&bins[0])
        .clone();
    let npm = format!("{name}@{version}");
    if !aaf_runtime::catalog::valid_npm(&npm)
        || !pick
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
    {
        return Err("npm returned an unexpected name".into());
    }
    Ok(serde_json::json!({"id": id, "label": name, "npm": npm, "acp": pick, "bins": bins}))
}
