//! ACP agent 目錄（源自先前的 Python 版）：解析指令、是否安裝、登入檢查、
//! 探測（initialize＋session/new，不送提示）、模型切換規則、實測模型。
//! 轉接器裝在 `<instance>/vendor/acp`；自訂 agent 記在 `<instance>/vendor/acp/custom.json`。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: String,
    pub label: String,
    /// ACP 指令（第一個元素是執行檔名）
    pub acp: Vec<String>,
    /// path＝在 PATH 找；vendor＝在 vendor/acp/node_modules/.bin 找（custom.json 可省略）
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    pub needs: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_cmd: Option<Vec<String>>,
    /// 原生 CLI 執行檔名（例：claude）；用來分辨「CLI 有、只缺 ACP 轉接器」
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<String>,
    #[serde(default)]
    pub custom: bool,
    /// 轉接器實際驅動的 agent 本體
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
}

/// agent 本體：內建在轉接器裡（bundled），或本機另外安裝的 CLI（cli）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Engine {
    /// bundled／cli
    pub source: String,
    /// bundled：vendor 裡的 npm 套件；cli＋npm 來源：CLI 的 npm 套件
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// bundled：package.json 欄位（預設 version）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// cli：查版本的指令（第一個元素是執行檔名或絕對路徑）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_cmd: Option<Vec<String>>,
    /// cli：叫轉接器改用指定本體的環境變數（值＝本體的絕對路徑）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// 官方最新版來源：`npm:<pkg>`／`github:<owner>/<repo>`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
}

fn e(
    id: &str,
    label: &str,
    acp: &[&str],
    source: &str,
    npm: Option<&str>,
    needs: &str,
    auth: Option<&[&str]>,
) -> Entry {
    Entry {
        id: id.into(),
        label: label.into(),
        acp: acp.iter().map(|s| s.to_string()).collect(),
        source: source.into(),
        npm: npm.map(Into::into),
        needs: needs.into(),
        auth_cmd: auth.map(|a| a.iter().map(|s| s.to_string()).collect()),
        cli: None,
        custom: false,
        engine: None,
    }
}

impl Entry {
    fn with_cli(mut self, cli: &str) -> Self {
        self.cli = Some(cli.into());
        self
    }

    fn with_engine(mut self, e: Engine) -> Self {
        self.engine = Some(e);
        self
    }
}

/// 內建目錄（npm 版本固定）。
pub fn builtin() -> Vec<Entry> {
    vec![
        e(
            "hermes",
            "Hermes",
            &["hermes-acp"],
            "path",
            None,
            "Hermes Agent（hermes-acp 隨 Hermes 安裝）",
            Some(&["hermes", "auth", "list"]),
        )
        .with_cli("hermes")
        .with_engine(cli_engine("hermes", None, None)),
        e(
            "claude",
            "Claude Code",
            &["claude-agent-acp"],
            "vendor",
            Some("@agentclientprotocol/claude-agent-acp@0.93.0"),
            "Claude Code 已登入（claude auth login）",
            Some(&["claude", "auth", "status"]),
        )
        .with_engine(bundled(
            "@anthropic-ai/claude-agent-sdk",
            Some("claudeCodeVersion"),
        ))
        .with_cli("claude"),
        e(
            "codex",
            "Codex",
            &["codex-acp"],
            "vendor",
            Some("@agentclientprotocol/codex-acp@2.2.2"),
            "Codex CLI 已登入（codex login）",
            Some(&["codex", "login", "status"]),
        )
        .with_engine(bundled("@openai/codex", None))
        .with_cli("codex"),
        e(
            "pi",
            "pi",
            &["pi-acp"],
            "vendor",
            Some("pi-acp@0.0.34"),
            "pi 已安裝並設定模型（~/.pi/agent/models.json）",
            None,
        )
        .with_cli("pi")
        .with_engine(cli_engine(
            "pi",
            Some("PI_ACP_PI_COMMAND"),
            Some("npm:@earendil-works/pi-coding-agent"),
        )),
        e(
            "agy",
            "Antigravity",
            &["agy-acp"],
            "vendor",
            Some("agy-acp@0.5.2"),
            "Antigravity CLI（agy）已安裝並登入；每輪較慢，agy 自己升級後需重測相容",
            None,
        )
        .with_cli("agy")
        .with_engine(cli_engine(
            "agy",
            Some("AGY_BIN"),
            Some("github:google-antigravity/antigravity-cli"),
        )),
    ]
}

fn bundled(pkg: &str, field: Option<&str>) -> Engine {
    Engine {
        source: "bundled".into(),
        package: Some(pkg.into()),
        field: field.map(Into::into),
        ..Default::default()
    }
}

fn cli_engine(bin: &str, env: Option<&str>, latest: Option<&str>) -> Engine {
    Engine {
        source: "cli".into(),
        package: latest.and_then(|l| l.strip_prefix("npm:")).map(Into::into),
        version_cmd: Some(vec![bin.into(), "--version".into()]),
        env: env.map(Into::into),
        latest: latest.map(Into::into),
        ..Default::default()
    }
}

/// 本體執行檔：絕對路徑照用，否則在常見位置找。
pub fn engine_bin(e: &Engine) -> Option<PathBuf> {
    let name = e.version_cmd.as_ref()?.first()?;
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.exists().then_some(p);
    }
    which(name)
}

/// 啟動轉接器時要帶的本體環境變數（例：AGY_BIN=/…/agy），讓轉接器用到的本體與 aaf 看到的一致。
pub fn engine_env(id: &str, instance: &Path) -> Vec<(String, String)> {
    let Some(e) = get(id, instance).and_then(|x| x.engine) else {
        return vec![];
    };
    match (&e.env, engine_bin(&e)) {
        (Some(k), Some(p)) if e.source == "cli" => vec![(k.clone(), p.display().to_string())],
        _ => vec![],
    }
}

/// 字串中第一個 x.y(.z…) 版本號。
pub fn semver_in(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit()
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'v')
        {
            let st = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'-') {
                i += 1;
            }
            let v = s[st..i].trim_end_matches(['.', '-']);
            if v.contains('.') {
                return Some(v.to_string());
            }
        }
        i += 1;
    }
    None
}

/// 本體版本：bundled 讀 `<root>/node_modules/<pkg>/package.json`；cli 跑 version_cmd（3 秒逾時）。
pub async fn engine_version(e: &Engine, root: &Path) -> Option<String> {
    if e.source == "bundled" {
        let t = std::fs::read_to_string(
            root.join("node_modules")
                .join(e.package.as_deref()?)
                .join("package.json"),
        )
        .ok()?;
        let v: Value = serde_json::from_str(&t).ok()?;
        return v[e.field.as_deref().unwrap_or("version")]
            .as_str()
            .map(Into::into);
    }
    let cmd = e.version_cmd.as_ref()?;
    let exe = engine_bin(e)?;
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(exe)
            .args(&cmd[1..])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    semver_in(
        &(String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)),
    )
}

/// 曾在內建目錄、已移除的 agent id（舊 roles.json 仍可能指到它們）。
pub const REMOVED: &[&str] = &["gemini"];

/// 解析失敗時給人看的原因：已移除的內建 agent 與「沒安裝」分開說。
pub fn unavailable_reason(id: &str, instance: &Path) -> String {
    if get(id, instance).is_none() && REMOVED.contains(&id) {
        format!("agent {id} was removed from the catalog; pick another agent for this role")
    } else {
        format!("agent {id} is not installed")
    }
}

pub fn vendor_dir(instance: &Path) -> PathBuf {
    instance.join("vendor").join("acp")
}

pub fn custom_file(instance: &Path) -> PathBuf {
    vendor_dir(instance).join("custom.json")
}

pub fn custom(instance: &Path) -> Vec<Entry> {
    let Ok(t) = std::fs::read_to_string(custom_file(instance)) else {
        return vec![];
    };
    // 一筆寫錯不連累其他筆
    let raw: Vec<Value> = serde_json::from_str(&t).unwrap_or_default();
    let mut v: Vec<Entry> = raw
        .into_iter()
        .filter_map(|x| match serde_json::from_value::<Entry>(x) {
            Ok(e) => Some(e),
            Err(err) => {
                tracing::warn!(error = %err, "custom.json: skipping an invalid agent entry");
                None
            }
        })
        .collect();
    for x in &mut v {
        x.custom = true;
        x.source = "vendor".into();
    }
    v
}

pub fn all(instance: &Path) -> Vec<Entry> {
    let mut v = builtin();
    for c in custom(instance) {
        if !v.iter().any(|b| b.id == c.id) {
            v.push(c);
        }
    }
    v
}

pub fn get(id: &str, instance: &Path) -> Option<Entry> {
    all(instance).into_iter().find(|e| e.id == id)
}

/// 本人常見的安裝位置（launchd 下 PATH 很短）。
pub fn search_path() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut v: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for extra in [
        home.join(".local/bin"),
        home.join(".hermes/hermes-agent/.hermes/bin"),
        // pi 官方安裝程式的位置（只寫進 .bashrc，服務的 PATH 看不到）
        home.join(".pi/agent/bin"),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
    ] {
        if !v.contains(&extra) {
            v.push(extra);
        }
    }
    v
}

pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if name.contains('/') {
        return None;
    }
    search_path().into_iter().map(|d| d.join(name)).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// 解析 ACP 指令（絕對路徑）；沒安裝回 None。
pub fn resolve_cmd(id: &str, instance: &Path) -> Option<Vec<String>> {
    let ent = get(id, instance)?;
    let exe = ent.acp.first()?;
    let path = if ent.source == "vendor" {
        let p = vendor_dir(instance).join("node_modules/.bin").join(exe);
        p.exists().then_some(p)?
    } else {
        which(exe)?
    };
    let mut v = vec![path.to_string_lossy().to_string()];
    v.extend(ent.acp[1..].iter().cloned());
    Some(v)
}

/// 已改名的轉接器套件：(agent, 舊套件)。升級時改裝目錄中的新套件並移除舊的。
pub const LEGACY_NPM: &[(&str, &str)] = &[("codex", "@zed-industries/codex-acp")];

/// `name@ver` 拆成 (name, ver)；scope 的 `@` 不算。
pub fn split_npm(spec: &str) -> (&str, Option<&str>) {
    match spec.rfind('@') {
        Some(i) if i > 0 => (&spec[..i], Some(&spec[i + 1..])),
        _ => (spec, None),
    }
}

/// vendor 裡實際安裝的版本（讀 node_modules/<name>/package.json）。
pub fn installed_version(instance: &Path, name: &str) -> Option<String> {
    let t = std::fs::read_to_string(
        vendor_dir(instance)
            .join("node_modules")
            .join(name)
            .join("package.json"),
    )
    .ok()?;
    serde_json::from_str::<Value>(&t).ok()?["version"]
        .as_str()
        .map(Into::into)
}

/// npm 套件名稱（可帶 scope 與版本）；嚴格檢查，子程序不經 shell。
pub fn valid_npm(s: &str) -> bool {
    let (name, ver) = match s.rfind('@') {
        Some(i) if i > 0 => (&s[..i], Some(&s[i + 1..])),
        _ => (s, None),
    };
    let ok_part = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-._".contains(c))
            && p.starts_with(|c: char| c.is_ascii_alphanumeric())
    };
    let name_ok = match name.strip_prefix('@') {
        Some(rest) => rest
            .split_once('/')
            .is_some_and(|(a, b)| ok_part(a) && ok_part(b)),
        None => ok_part(name),
    };
    name_ok
        && ver.is_none_or(|v| {
            !v.is_empty()
                && v.len() <= 40
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
        })
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    #[serde(flatten)]
    pub entry: Entry,
    /// ACP 指令解析得到＝可以跑（相容舊欄位；與 adapter_installed 同值）
    pub installed: bool,
    pub adapter_installed: bool,
    /// 原生 CLI 找得到；目錄沒標 cli 的 agent 為 null
    pub cli_installed: Option<bool>,
    pub command: Option<Vec<String>>,
    /// true＝已登入；false＝未登入；null＝無法判斷
    pub logged_in: Option<bool>,
    pub login_note: String,
}

/// 登入檢查：跑 auth_cmd（短逾時、限長）；pi 看設定檔。
pub async fn auth_check(ent: &Entry) -> (Option<bool>, String) {
    auth_check_with(ent, &which).await
}

async fn auth_check_with(
    ent: &Entry,
    find: &(dyn Fn(&str) -> Option<PathBuf> + Sync),
) -> (Option<bool>, String) {
    if ent.id == "pi" {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let ok = home.join(".pi/agent/models.json").exists();
        return (
            Some(ok),
            if ok {
                "models.json found".into()
            } else {
                "~/.pi/agent/models.json not found".into()
            },
        );
    }
    let Some(cmd) = &ent.auth_cmd else {
        return (None, "cannot check login for this agent".into());
    };
    let Some(exe) = find(&cmd[0]) else {
        return (Some(false), format!("{} not found", cmd[0]));
    };
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(exe)
            .args(&cmd[1..])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match out {
        Err(_) => (None, "login check timed out".into()),
        Ok(Err(e)) => (None, format!("login check failed: {e}")),
        Ok(Ok(o)) => {
            let text = String::from_utf8_lossy(&o.stdout).to_lowercase()
                + &String::from_utf8_lossy(&o.stderr).to_lowercase();
            let neg = [
                "not logged",
                "not authenticated",
                "no credentials",
                "\"loggedin\": false",
                "\"loggedin\":false",
                "no auth",
            ];
            if !o.status.success() || neg.iter().any(|n| text.contains(n)) {
                (Some(false), "not logged in".into())
            } else {
                (Some(true), "logged in".into())
            }
        }
    }
}

pub async fn status(instance: &Path) -> Vec<Status> {
    status_with(instance, &which).await
}

/// `find` 找原生 CLI 與登入檢查指令（測試可注入）。轉接器或 CLI 任一在就跑登入檢查。
async fn status_with(
    instance: &Path,
    find: &(dyn Fn(&str) -> Option<PathBuf> + Sync),
) -> Vec<Status> {
    let mut out = Vec::new();
    for ent in all(instance) {
        let command = resolve_cmd(&ent.id, instance);
        let cli_installed = ent.cli.as_deref().map(|c| find(c).is_some());
        let (logged_in, login_note) = if command.is_some() || cli_installed == Some(true) {
            auth_check_with(&ent, find).await
        } else {
            (None, String::new())
        };
        out.push(Status {
            installed: command.is_some(),
            adapter_installed: command.is_some(),
            cli_installed,
            command,
            logged_in,
            login_note,
            entry: ent,
        });
    }
    out
}

/// 模型切換規則（configOptions category=model 優先；否則 availableModels）。
/// 回 Ok(None)＝不需切換或 agent 不回報模型；Err＝清單裡沒有。
pub fn model_switch(
    session: &Value,
    want: &str,
) -> Result<Option<(String, Value, String)>, String> {
    let matches = |id: &str| {
        id == want
            || id.split_once(':').is_some_and(|(_, b)| b == want)
            || id.split_once('/').is_some_and(|(_, b)| b == want)
    };
    if let Some(opt) = session["configOptions"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["category"] == "model"))
    {
        let vals: Vec<&str> = opt["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o["value"].as_str())
            .collect();
        let target = vals.iter().find(|v| matches(v)).ok_or_else(|| {
            format!(
                "this agent has no model {want} (available: {})",
                vals.iter().take(8).copied().collect::<Vec<_>>().join(", ")
            )
        })?;
        if opt["currentValue"].as_str() == Some(target) {
            return Ok(None);
        }
        return Ok(Some((
            "session/set_config_option".into(),
            json!({"configId": opt["id"], "value": target}),
            target.to_string(),
        )));
    }
    let ids: Vec<&str> = session["models"]["availableModels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["modelId"].as_str())
        .collect();
    if ids.is_empty() {
        return Ok(None);
    }
    let target = ids.iter().find(|v| matches(v)).ok_or_else(|| {
        format!(
            "this agent has no model {want} (available: {})",
            ids.iter().take(8).copied().collect::<Vec<_>>().join(", ")
        )
    })?;
    if session["models"]["currentModelId"].as_str() == Some(target) {
        return Ok(None);
    }
    Ok(Some((
        "session/set_model".into(),
        json!({"modelId": target}),
        target.to_string(),
    )))
}

/// 推理強度（configOptions category=thought_level）：(設定 id, 目前值, 可選值)。沒有這個選項回 None。
pub fn efforts_of(session: &Value) -> Option<(String, Option<String>, Vec<String>)> {
    let opt = session["configOptions"]
        .as_array()?
        .iter()
        .find(|o| o["category"] == "thought_level")?;
    let vals = opt["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["value"].as_str().map(String::from))
        .collect();
    Some((
        opt["id"].as_str()?.to_string(),
        opt["currentValue"].as_str().map(String::from),
        vals,
    ))
}

/// 切推理強度的請求；Err＝這個 agent／模型不提供該值。
pub fn effort_switch(session: &Value, want: &str) -> Result<Option<(String, Value)>, String> {
    let Some((id, current, vals)) = efforts_of(session) else {
        return Err(format!(
            "this agent has no reasoning effort setting (wanted {want})"
        ));
    };
    if !vals.iter().any(|v| v == want) {
        return Err(format!(
            "this agent/model has no effort {want} (available: {})",
            vals.join(", ")
        ));
    }
    if current.as_deref() == Some(want) {
        return Ok(None);
    }
    Ok(Some((
        "session/set_config_option".into(),
        json!({"configId": id, "value": want}),
    )))
}

/// session/new 回應中的模型清單與目前模型。
pub fn models_of(session: &Value) -> (Option<String>, Vec<Value>) {
    if let Some(opt) = session["configOptions"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["category"] == "model"))
    {
        let list = opt["options"].as_array().into_iter().flatten().filter_map(|o| Some(json!({"id": o["value"].as_str()?, "name": o["name"].as_str().or(o["value"].as_str())}))).collect();
        return (opt["currentValue"].as_str().map(Into::into), list);
    }
    let list = session["models"]["availableModels"].as_array().into_iter().flatten().filter_map(|m| Some(json!({"id": m["modelId"].as_str()?, "name": m["name"].as_str().or(m["modelId"].as_str())}))).collect();
    (
        session["models"]["currentModelId"].as_str().map(Into::into),
        list,
    )
}

/// 權限模式由寬到窄（YOLO 選第一個 agent 有的）。各 agent 實測：claude bypassPermissions、
/// codex agent-full-access、hermes dont_ask；pi 沒有權限模式。
pub const WIDEST: &str = "@widest";
const WIDEST_ORDER: &[&str] = &[
    "bypassPermissions",
    "agent-full-access",
    "full-access",
    "yolo",
    "dont_ask",
    "dontAsk",
    "auto",
    "acceptEdits",
    "accept_edits",
];

/// agent 回報的權限模式：(目前, [{id, name, description}])。configOptions category=mode 優先，其次 modes。
pub fn modes_of(session: &Value) -> (Option<String>, Vec<Value>) {
    if let Some(opt) = session["configOptions"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["category"] == "mode"))
    {
        let descs: std::collections::HashMap<&str, &Value> = session["modes"]["availableModes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| Some((m["id"].as_str()?, &m["description"])))
            .collect();
        let list = opt["options"].as_array().into_iter().flatten().filter_map(|o| {
            let id = o["value"].as_str()?;
            Some(json!({"id": id, "name": o["name"].as_str().unwrap_or(id), "description": descs.get(id).copied().cloned().unwrap_or(Value::Null)}))
        }).collect();
        return (opt["currentValue"].as_str().map(Into::into), list);
    }
    let list = session["modes"]["availableModes"].as_array().into_iter().flatten().filter_map(|m| {
        let id = m["id"].as_str()?;
        Some(json!({"id": id, "name": m["name"].as_str().unwrap_or(id), "description": m["description"]}))
    }).collect();
    (
        session["modes"]["currentModeId"].as_str().map(Into::into),
        list,
    )
}

/// 某 agent 回報清單中最寬的模式。
pub fn widest_mode(ids: &[&str]) -> Option<String> {
    WIDEST_ORDER
        .iter()
        .find(|w| ids.contains(w))
        .map(|w| w.to_string())
}

/// 續接 session（沒有清單）時依序嘗試的模式。
pub fn mode_candidates(want: &str) -> Vec<String> {
    if want == WIDEST {
        WIDEST_ORDER.iter().map(|s| s.to_string()).collect()
    } else {
        vec![want.to_string()]
    }
}

/// 新 session：把想要的模式換成 (method, params, 實際 id)；agent 沒有這個模式就回 None（不擋回合）。
pub fn mode_switch(session: &Value, want: &str) -> Option<(String, Value, String)> {
    let (cur, list) = modes_of(session);
    let ids: Vec<&str> = list.iter().filter_map(|m| m["id"].as_str()).collect();
    let target = if want == WIDEST {
        widest_mode(&ids)?
    } else {
        ids.iter().find(|i| **i == want)?.to_string()
    };
    if cur.as_deref() == Some(target.as_str()) {
        return None;
    }
    if let Some(opt) = session["configOptions"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["category"] == "mode"))
    {
        return Some((
            "session/set_config_option".into(),
            json!({"configId": opt["id"], "value": target}),
            target,
        ));
    }
    Some(("session/set_mode".into(), json!({"modeId": target}), target))
}

/// 一次性 ACP 連線：探測與實測模型用。程序以 process group 啟動，結束時整組殺掉。
struct Oneshot {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    text: String,
}

impl Oneshot {
    fn spawn(cmd: &[String], env: &[(String, String)], cwd: &Path) -> std::io::Result<Self> {
        use tokio::io::AsyncBufReadExt;
        let mut c = tokio::process::Command::new(&cmd[0]);
        c.args(&cmd[1..])
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = c.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        Ok(Self {
            child,
            stdin,
            lines,
            text: String::new(),
        })
    }

    async fn call(&mut self, id: u64, method: &str, params: Value) -> Result<Value, String> {
        use tokio::io::AsyncWriteExt;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string()
            + "\n";
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        loop {
            let Some(l) = self.lines.next_line().await.map_err(|e| e.to_string())? else {
                return Err("agent exited".into());
            };
            let Ok(m) = serde_json::from_str::<Value>(&l) else {
                continue;
            };
            if m["method"] == "session/update" {
                let u = &m["params"]["update"];
                if u["sessionUpdate"] == "agent_message_chunk" {
                    self.text
                        .push_str(u["content"]["text"].as_str().unwrap_or_default());
                }
                continue;
            }
            if m.get("method").is_some() && m.get("id").is_some() {
                // agent 的請求（權限等）：一律拒絕／取消，探測不做任何事
                let resp = if m["method"] == "session/request_permission" {
                    json!({"jsonrpc": "2.0", "id": m["id"], "result": {"outcome": {"outcome": "cancelled"}}})
                } else {
                    json!({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "not supported"}})
                };
                let _ = self
                    .stdin
                    .write_all((resp.to_string() + "\n").as_bytes())
                    .await;
                continue;
            }
            if m["id"] == json!(id) {
                if let Some(e) = m.get("error") {
                    return Err(e.to_string().chars().take(300).collect());
                }
                return Ok(m["result"].clone());
            }
        }
    }

    fn kill(&mut self) {
        if let Some(pid) = self.child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

impl Drop for Oneshot {
    fn drop(&mut self) {
        self.kill();
    }
}

fn init_params() -> Value {
    json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}, "clientInfo": {"name": "aaf", "version": env!("CARGO_PKG_VERSION")}})
}

/// 探測：initialize＋session/new，不送提示（不花 token）。回 {ok, model, models, error}。
pub async fn probe(cmd: &[String], cwd: &Path, timeout: Duration) -> Value {
    probe_env(cmd, &[], cwd, timeout).await
}

pub async fn probe_env(
    cmd: &[String],
    env: &[(String, String)],
    cwd: &Path,
    timeout: Duration,
) -> Value {
    let run = async {
        let mut o = Oneshot::spawn(cmd, env, cwd).map_err(|e| format!("cannot start: {e}"))?;
        let init = o.call(1, "initialize", init_params()).await?;
        let new = o
            .call(2, "session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await?;
        let (model, models) = models_of(&new);
        let (mode, modes) = modes_of(&new);
        let (effort, efforts) = efforts_of(&new).map(|(_, c, v)| (c, v)).unwrap_or_default();
        Ok::<_, String>(
            json!({"ok": true, "agent": init["agentInfo"], "model": model, "models": models, "mode": mode, "modes": modes, "effort": effort, "efforts": efforts}),
        )
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({"ok": false, "error": e}),
        Err(_) => json!({"ok": false, "error": format!("timed out after {}s", timeout.as_secs())}),
    }
}

/// 只驗連線：initialize＋session/new＋（指定模型時）確認清單有它並切過去；不送提示（不花 token）。
/// 回 {ok, model, connect_only: true, secs}。模型在清單裡但帳號叫不動這類問題要實際派工或 check_model 才看得到。
/// `effort` 非空時也確認切到該模型後有這個推理強度，並切過去。
pub async fn check_connect(
    cmd: &[String],
    env: &[(String, String)],
    cwd: &Path,
    model: &str,
    effort: &str,
    timeout: Duration,
) -> Value {
    let started = std::time::Instant::now();
    let run = async {
        let mut o = Oneshot::spawn(cmd, env, cwd).map_err(|e| format!("cannot start: {e}"))?;
        o.call(1, "initialize", init_params()).await?;
        let mut new = o
            .call(2, "session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await?;
        let (current, _) = models_of(&new);
        let mut target = if model.is_empty() {
            current.unwrap_or_default()
        } else {
            model.to_string()
        };
        if !model.is_empty() {
            if let Some((method, mut params, t)) = model_switch(&new, model)? {
                params["sessionId"] = new["sessionId"].clone();
                let r = o.call(3, &method, params).await?;
                // 推理強度的選項可能隨模型不同：用切換後回報的設定
                if r["configOptions"].is_array() {
                    new["configOptions"] = r["configOptions"].clone();
                }
                target = t;
            }
        }
        if !effort.is_empty() {
            if let Some((method, mut params)) = effort_switch(&new, effort)? {
                params["sessionId"] = new["sessionId"].clone();
                o.call(4, &method, params).await?;
            }
        }
        let mut v = json!({"ok": true, "model": target, "connect_only": true});
        if !effort.is_empty() {
            v["effort"] = json!(effort);
        }
        Ok::<_, String>(v)
    };
    let mut v = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({"ok": false, "error": e, "connect_only": true}),
        Err(_) => {
            json!({"ok": false, "error": format!("timed out after {}s", timeout.as_secs()), "connect_only": true})
        }
    };
    v["secs"] = json!((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    v
}

/// 實測模型：暫時 session、切模型（model 空字串＝用 agent 預設）、送「只回 OK」。回 {ok, model, error}（花一次極小呼叫的 token）。
pub async fn check_model(cmd: &[String], cwd: &Path, model: &str, timeout: Duration) -> Value {
    check_model_env(cmd, &[], cwd, model, timeout).await
}

/// 同 check_model，另帶環境變數（例：叫轉接器用暫存目錄裡的新本體）。回傳多帶 secs。
pub async fn check_model_env(
    cmd: &[String],
    env: &[(String, String)],
    cwd: &Path,
    model: &str,
    timeout: Duration,
) -> Value {
    let started = std::time::Instant::now();
    let run = async {
        let mut o = Oneshot::spawn(cmd, env, cwd).map_err(|e| format!("cannot start: {e}"))?;
        o.call(1, "initialize", init_params()).await?;
        let new = o
            .call(2, "session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await?;
        let sid = new["sessionId"].clone();
        let (current, _) = models_of(&new);
        let mut target = if model.is_empty() {
            current.unwrap_or_default()
        } else {
            model.to_string()
        };
        let switch = if model.is_empty() {
            None
        } else {
            model_switch(&new, model)?
        };
        if let Some((method, mut params, t)) = switch {
            params["sessionId"] = sid.clone();
            o.call(3, &method, params).await?;
            target = t;
        }
        let res = o.call(4, "session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "Reply with exactly: OK"}]})).await?;
        let stop = res["stopReason"].as_str().unwrap_or("end_turn");
        if stop != "end_turn" {
            return Err(format!("stopReason={stop}"));
        }
        if let Some(err) = provider_error(&o.text) {
            return Err(err);
        }
        Ok::<_, String>(
            json!({"ok": true, "model": target, "reply": o.text.chars().take(200).collect::<String>()}),
        )
    };
    let mut v = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({"ok": false, "error": e}),
        Err(_) => json!({"ok": false, "error": format!("timed out after {}s", timeout.as_secs())}),
    };
    v["secs"] = json!((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    v
}

/// 有些 agent 把供應商錯誤當成一般回覆文字送回來。
pub fn provider_error(text: &str) -> Option<String> {
    let t = text.trim();
    let l = t.to_lowercase();
    let hit = [
        "api error",
        "rate limit",
        "quota exceeded",
        "insufficient_quota",
        "model not found",
        "not supported for",
        "unauthorized",
        "invalid api key",
        "model_not_found",
        "you have exceeded",
    ];
    (t.len() < 600 && hit.iter().any(|h| l.contains(h))).then(|| t.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_path_has_pi_installer_dir() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        assert!(search_path().contains(&home.join(".pi/agent/bin")));
    }

    #[test]
    fn npm_names() {
        for ok in [
            "pi-acp",
            "pi-acp@0.0.34",
            "@agentclientprotocol/codex-acp@2.2.2",
            "@agentclientprotocol/claude-agent-acp",
        ] {
            assert!(valid_npm(ok), "{ok}");
        }
        for bad in [
            "",
            "Foo",
            "a b",
            "../x",
            "@x",
            "@/x",
            "x@",
            "x;rm -rf /",
            "-g",
            ".hidden",
            "https://evil/x.tgz",
            "git+ssh://x",
            "x@1.0 && y",
            "file:../x",
        ] {
            assert!(!valid_npm(bad), "{bad}");
        }
    }

    #[test]
    fn switch_rules() {
        let cfg = json!({"configOptions": [{"id": "model", "category": "model", "currentValue": "a/one", "options": [{"value": "a/one"}, {"value": "b/two"}]}],
                         "models": {"availableModels": [{"modelId": "ignored"}]}});
        assert_eq!(model_switch(&cfg, "one").unwrap(), None);
        assert_eq!(
            model_switch(&cfg, "two").unwrap().unwrap().0,
            "session/set_config_option"
        );
        assert!(model_switch(&cfg, "three")
            .unwrap_err()
            .contains("a/one, b/two"));
        let av = json!({"models": {"currentModelId": "x", "availableModels": [{"modelId": "x"}, {"modelId": "p:y"}]}});
        let (m, p, t) = model_switch(&av, "y").unwrap().unwrap();
        assert_eq!(
            (m.as_str(), p["modelId"].as_str(), t.as_str()),
            ("session/set_model", Some("p:y"), "p:y")
        );
        assert_eq!(model_switch(&av, "x").unwrap(), None);
        assert_eq!(
            model_switch(&json!({}), "any").unwrap(),
            None,
            "agent does not report models"
        );
        assert_eq!(models_of(&av).1.len(), 2);
    }

    #[test]
    fn resolve_vendor_and_custom() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(resolve_cmd("codex", d.path()), None);
        let bin = vendor_dir(d.path()).join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("codex-acp"), "").unwrap();
        assert!(resolve_cmd("codex", d.path()).unwrap()[0].ends_with("node_modules/.bin/codex-acp"));
        std::fs::write(custom_file(d.path()), r#"[{"id":"mine","label":"Mine","acp":["mine-acp","--x"],"source":"path","npm":"mine-acp@1.0.0","needs":""}]"#).unwrap();
        std::fs::write(bin.join("mine-acp"), "").unwrap();
        assert_eq!(
            resolve_cmd("mine", d.path()).unwrap()[1],
            "--x",
            "custom entries always resolve from vendor"
        );
        assert!(get("mine", d.path()).unwrap().custom);
        assert_eq!(resolve_cmd("nope", d.path()), None);
    }

    #[test]
    fn gemini_removed() {
        let d = tempfile::tempdir().unwrap();
        assert!(get("gemini", d.path()).is_none());
        assert!(unavailable_reason("gemini", d.path()).contains("removed"));
        assert!(unavailable_reason("codex", d.path()).contains("not installed"));
        // custom.json 自己定義的 gemini 照常可用，不算已移除
        std::fs::create_dir_all(vendor_dir(d.path())).unwrap();
        std::fs::write(custom_file(d.path()), r#"[{"id":"gemini","label":"G","acp":["gemini","--acp"],"source":"vendor","needs":""}]"#).unwrap();
        assert!(get("gemini", d.path()).unwrap().custom);
        assert!(unavailable_reason("gemini", d.path()).contains("not installed"));
    }

    /// 假 CLI：登入檢查指令 `<name>` 換成 /bin/echo 或 /usr/bin/false。
    fn fake_find(cli_ok: bool) -> impl Fn(&str) -> Option<PathBuf> + Sync {
        move |name: &str| match name {
            "claude" if cli_ok => Some(PathBuf::from("/bin/echo")),
            "codex" if cli_ok => Some(PathBuf::from("/usr/bin/false")),
            _ => None,
        }
    }

    #[tokio::test]
    async fn status_layers() {
        let d = tempfile::tempdir().unwrap();
        let by = |v: &[Status], id: &str| v.iter().find(|s| s.entry.id == id).unwrap().clone();
        // 什麼都沒有：不跑登入檢查
        let st = status_with(d.path(), &fake_find(false)).await;
        let c = by(&st, "claude");
        assert_eq!(
            (
                c.installed,
                c.adapter_installed,
                c.cli_installed,
                c.logged_in
            ),
            (false, false, Some(false), None)
        );
        assert_eq!(
            st.iter().map(|s| s.entry.id.as_str()).collect::<Vec<_>>(),
            ["hermes", "claude", "codex", "pi", "agy"]
        );
        // CLI 在、轉接器缺：照樣檢查登入（echo 成功＝已登入，false 失敗＝未登入）
        let st = status_with(d.path(), &fake_find(true)).await;
        let c = by(&st, "claude");
        assert_eq!(
            (c.adapter_installed, c.cli_installed, c.logged_in),
            (false, Some(true), Some(true))
        );
        let x = by(&st, "codex");
        assert_eq!(
            (x.adapter_installed, x.cli_installed, x.logged_in),
            (false, Some(true), Some(false))
        );
        // 轉接器裝好：installed 與 adapter_installed 同為 true
        let bin = vendor_dir(d.path()).join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("claude-agent-acp"), "").unwrap();
        let c = by(&status_with(d.path(), &fake_find(true)).await, "claude");
        assert_eq!(
            (
                c.installed,
                c.adapter_installed,
                c.cli_installed,
                c.logged_in
            ),
            (true, true, Some(true), Some(true))
        );
        // custom 沒標 cli：cli_installed 為 null
        std::fs::write(
            custom_file(d.path()),
            r#"[{"id":"mine","label":"M","acp":["mine-acp"],"source":"vendor","needs":""}]"#,
        )
        .unwrap();
        assert_eq!(
            by(&status_with(d.path(), &fake_find(true)).await, "mine").cli_installed,
            None
        );
    }
}
