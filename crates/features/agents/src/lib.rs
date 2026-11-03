//! Agents 管理（源自先前的 Python 版）。
//!
//! 全部 Owner 限定。roles.json 是單一真相：這裡只負責原子寫檔，再呼叫 chat 的熱重載套用
//! （新角色發 token、改引擎／模型的角色收尾後套用並遺忘舊 session）。

use aaf_api::error::{ApiError, ApiResult};
use aaf_api::{AppState, HttpFeature, User};
use aaf_core::perms::Rank;
use aaf_core::storage::Migration;
use aaf_core::{Core, Feature};
use aaf_runtime::catalog;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod accounts_api;
pub mod compat;
pub mod engine;
pub mod install;
pub mod quota;
pub mod roles;
pub mod turns;

pub struct Agents {
    pub(crate) inner: Arc<Inner>,
}

pub struct Inner {
    instance: PathBuf,
    chat: Arc<aaf_feature_chat::Shared>,
    jobs: install::Jobs,
    /// npm 執行檔（測試可換成假的）
    npm: Mutex<Option<PathBuf>>,
    probe_timeout: Mutex<Duration>,
    models_cache: Mutex<HashMap<String, (Instant, Value)>>,
    /// 檢查更新的結果：npm 套件名 → {latest, deprecated, at, error}
    latest_cache: Mutex<HashMap<String, Value>>,
    quota_cache: tokio::sync::Mutex<Option<(Instant, Value)>>,
}

impl Agents {
    pub fn new(instance: &std::path::Path, chat: Arc<aaf_feature_chat::Shared>) -> Self {
        Self {
            inner: Arc::new(Inner {
                instance: instance.to_path_buf(),
                chat,
                jobs: Default::default(),
                npm: Mutex::new(None),
                probe_timeout: Mutex::new(Duration::from_secs(120)),
                models_cache: Default::default(),
                latest_cache: Default::default(),
                quota_cache: Default::default(),
            }),
        }
    }

    /// 測試用：指定 npm 執行檔與探測逾時。
    pub fn with_npm(self, npm: PathBuf) -> Self {
        *self.inner.npm.lock().unwrap() = Some(npm);
        self
    }

    pub fn with_probe_timeout(self, t: Duration) -> Self {
        *self.inner.probe_timeout.lock().unwrap() = t;
        self
    }
}

impl Inner {
    fn npm(&self) -> ApiResult<PathBuf> {
        self.npm
            .lock()
            .unwrap()
            .clone()
            .or_else(|| catalog::which("npm"))
            .ok_or_else(|| ApiError::BadRequest("npm not found; install Node.js first".into()))
    }

    fn work(&self) -> PathBuf {
        let d = self.instance.join("work").join("_probe");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    async fn apply(&self, core: &Core) -> ApiResult<Value> {
        let d = self
            .chat
            .reload(core)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        Ok(json!({"added": d.added, "removed": d.removed, "changed": d.changed}))
    }
}

type X = Extension<Arc<Inner>>;

fn owner(u: &User) -> ApiResult<()> {
    if u.rank != Rank::Owner {
        return Err(ApiError::Forbidden("agent management is owner-only".into()));
    }
    Ok(())
}

fn bad(e: impl std::fmt::Display) -> ApiError {
    ApiError::BadRequest(e.to_string())
}

async fn catalog_list(Extension(x): X, u: User) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let st = catalog::status(&x.instance).await;
    let roles = roles::read(&x.instance).map_err(bad)?;
    Ok(Json(
        json!({"agents": st, "roles": roles, "vendor": catalog::vendor_dir(&x.instance)}),
    ))
}

#[derive(Deserialize)]
struct PlanReq {
    npm: String,
}

async fn plan_custom(Extension(x): X, u: User, Json(b): Json<PlanReq>) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let npm = x.npm()?;
    let mut p = install::plan_custom(&npm, &b.npm).await.map_err(bad)?;
    if catalog::builtin().iter().any(|e| e.id == p["id"]) {
        return Err(ApiError::Conflict(format!(
            "{} is a built-in agent; install it from the list",
            p["id"]
        )));
    }
    p["install_cmd"] = json!([
        npm.to_string_lossy(),
        "install",
        "--no-audit",
        "--no-fund",
        "--save-exact",
        p["npm"]
    ]);
    Ok(Json(p))
}

#[derive(Deserialize)]
struct InstallReq {
    #[serde(default)]
    agent: Option<String>,
    /// 自訂：npm 套件（伺服器重查計畫，不信任前端內容）
    #[serde(default)]
    npm: Option<String>,
    #[serde(default)]
    confirm: bool,
}

async fn install_start(
    Extension(x): X,
    u: User,
    Json(b): Json<InstallReq>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    if !b.confirm {
        return Err(ApiError::BadRequest(
            "installing requires confirm: true".into(),
        ));
    }
    let npm = x.npm()?;
    let (agent, pkg, custom) = match (&b.agent, &b.npm) {
        (Some(a), None) => {
            let e = catalog::builtin()
                .into_iter()
                .find(|e| &e.id == a)
                .ok_or(ApiError::NotFound)?;
            let pkg = e
                .npm
                .ok_or_else(|| bad(format!("{a} is not installed by AA Forum ({})", e.needs)))?;
            (a.clone(), pkg, None)
        }
        (None, Some(spec)) => {
            let p = install::plan_custom(&npm, spec).await.map_err(bad)?;
            let id = p["id"].as_str().unwrap_or_default().to_string();
            if catalog::builtin().iter().any(|e| e.id == id) {
                return Err(ApiError::Conflict(format!("{id} is a built-in agent")));
            }
            let plan = install::CustomPlan {
                id: id.clone(),
                label: p["label"].as_str().unwrap_or(&id).into(),
                acp: p["acp"].as_str().unwrap_or_default().into(),
            };
            (
                id,
                p["npm"].as_str().unwrap_or_default().to_string(),
                Some(plan),
            )
        }
        _ => return Err(bad("give either agent or npm")),
    };
    let job = x
        .jobs
        .start(
            &x.instance,
            npm,
            &agent,
            &pkg,
            custom,
            Duration::from_secs(900),
        )
        .map_err(|e| {
            if e.contains("another install") {
                ApiError::Conflict(e)
            } else {
                bad(e)
            }
        })?;
    Ok(Json(json!(job)))
}

#[derive(Deserialize)]
struct Offset {
    #[serde(default)]
    offset: usize,
}

/// 目錄中所有 vendor 套件 agent：套件名、已安裝版本、建議版本、用到的角色。不連網。
async fn packages(Extension(x): X, u: User) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let file = roles::read(&x.instance).unwrap_or_default();
    let latest = x.latest_cache.lock().unwrap().clone();
    let mut out = vec![];
    for e in catalog::all(&x.instance) {
        let spec = e.npm.clone().filter(|_| e.source == "vendor");
        let (name, pinned) = match &spec {
            Some(s) => {
                let (n, v) = catalog::split_npm(s);
                (Some(n.to_string()), v.map(String::from))
            }
            None => (None, None),
        };
        let installed = name
            .as_deref()
            .and_then(|n| catalog::installed_version(&x.instance, n));
        let legacy: Vec<Value> = catalog::LEGACY_NPM
            .iter()
            .filter(|(a, _)| *a == e.id)
            .filter_map(|(_, p)| {
                catalog::installed_version(&x.instance, p).map(|v| json!({"npm": p, "version": v}))
            })
            .collect();
        let used_by = roles_using(&file, &e.id);
        let l = name
            .as_deref()
            .and_then(|n| latest.get(n).cloned())
            .unwrap_or(Value::Null);
        // 本體：版本、官方最新版、相容紀錄
        let (eng, compat) = match &e.engine {
            Some(en) => {
                let mut i = engine::info(&x.instance, en).await;
                let el = latest
                    .get(&format!("engine:{}", e.id))
                    .cloned()
                    .unwrap_or(Value::Null);
                i["latest"] = el["latest"].clone();
                i["latest_error"] = el["error"].clone();
                let adapter_ver = if e.source == "vendor" {
                    installed.clone()
                } else {
                    i["version"].as_str().map(String::from)
                };
                let c = compat::state(
                    &x.instance,
                    &e.id,
                    adapter_ver.as_deref(),
                    i["version"].as_str(),
                );
                (i, c)
            }
            None => (Value::Null, Value::Null),
        };
        out.push(json!({
            "agent": e.id, "label": e.label, "custom": e.custom, "npm": name,
            "pinned": pinned, "installed": installed, "legacy": legacy,
            "latest": l["latest"], "deprecated": l["deprecated"], "checked_at": l["at"],
            "check_error": l["error"], "used_by": used_by, "engine": eng, "compat": compat,
        }));
    }
    Ok(Json(json!({"packages": out})))
}

/// roles.json 裡（全域或任一群覆寫）用到這個 agent 的角色。
fn roles_using(file: &serde_json::Map<String, Value>, agent: &str) -> Vec<String> {
    let mut v: Vec<String> = file
        .iter()
        .filter(|(_, r)| {
            r["acp_agent"] == agent
                || r["room_overrides"]
                    .as_object()
                    .is_some_and(|o| o.values().any(|ov| ov["acp_agent"] == agent))
        })
        .map(|(k, _)| k.clone())
        .collect();
    v.sort();
    v
}

/// 檢查更新：對每個 vendor 套件跑 `npm view`（使用者按鈕觸發，不排程）。
async fn packages_check(Extension(x): X, u: User) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let npm = x.npm()?;
    let all = catalog::all(&x.instance);
    let names: Vec<String> = all
        .iter()
        .filter(|e| e.source == "vendor")
        .filter_map(|e| e.npm.clone())
        .map(|s| catalog::split_npm(&s).0.to_string())
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut fresh = HashMap::new();
    for n in names {
        let v = match install::npm_latest(&npm, &n).await {
            Ok((latest, dep)) => json!({"latest": latest, "deprecated": dep, "at": now}),
            Err(e) => json!({"error": e, "at": now}),
        };
        fresh.insert(n, v);
    }
    // 本體官方最新版（cli 本體才查；內建本體跟著轉接器走）
    for e in &all {
        let Some(en) = e.engine.as_ref().filter(|en| en.latest.is_some()) else {
            continue;
        };
        let v = match engine::latest(&npm, en).await {
            Ok(l) => json!({"latest": l, "at": now}),
            Err(err) => json!({"error": err, "at": now}),
        };
        fresh_engine(&x, &e.id, v);
    }
    x.latest_cache.lock().unwrap().extend(fresh);
    packages(Extension(x), u).await
}

fn fresh_engine(x: &Inner, id: &str, v: Value) {
    x.latest_cache
        .lock()
        .unwrap()
        .insert(format!("engine:{id}"), v);
}

/// 「測試相容」：用目前轉接器＋本體跑一句 OK，記進相容紀錄。
async fn compat_test(Extension(x): X, u: User, Json(b): Json<AgentReq>) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let e = catalog::get(&b.agent, &x.instance).ok_or(ApiError::NotFound)?;
    let cmd = cmd_for(&x, &b.agent)?;
    let env = catalog::engine_env(&b.agent, &x.instance);
    let t = *x.probe_timeout.lock().unwrap();
    let ev = match &e.engine {
        Some(en) => catalog::engine_version(en, &catalog::vendor_dir(&x.instance)).await,
        None => None,
    };
    let av = if e.source == "vendor" {
        e.npm
            .as_deref()
            .and_then(|s| catalog::installed_version(&x.instance, catalog::split_npm(s).0))
    } else {
        ev.clone()
    };
    let check = catalog::check_model_env(&cmd, &env, &x.work(), "", t).await;
    compat::record(
        &x.instance,
        &b.agent,
        av.as_deref(),
        ev.as_deref(),
        &check,
        "manual test",
    );
    Ok(Json(check))
}

#[derive(Deserialize)]
struct EngineUpReq {
    agent: String,
    version: String,
    #[serde(default)]
    confirm: bool,
}

/// 本體升級（agy、pi 這類本機 CLI）：暫存安裝＋用目前轉接器實測，通過才替換本機。
async fn engine_upgrade(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Json(b): Json<EngineUpReq>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    if !b.confirm {
        return Err(bad("upgrading requires confirm: true"));
    }
    let e = catalog::get(&b.agent, &x.instance).ok_or(ApiError::NotFound)?;
    let en = e
        .engine
        .clone()
        .ok_or_else(|| bad("this agent has no engine info"))?;
    let cmd = cmd_for(&x, &b.agent)?;
    let av = e
        .npm
        .as_deref()
        .and_then(|s| catalog::installed_version(&x.instance, catalog::split_npm(s).0));
    let (core, chat, agent) = (s.core.clone(), x.chat.clone(), e.id.clone());
    let after: install::After = Box::new(move || {
        Box::pin(async move {
            match chat.dispatcher(&core).restart_agent(&agent).await {
                Ok(l) if l.is_empty() => "no open sessions use this agent".into(),
                Ok(l) => format!(
                    "{} open session(s) restart with the new version after their current turn",
                    l.len()
                ),
                Err(e) => format!("restarting sessions failed: {e}"),
            }
        })
    });
    let job = x
        .jobs
        .start_engine_upgrade(
            &x.instance,
            x.npm()?,
            engine::EngineUpgrade {
                agent: e.id.clone(),
                engine: en,
                version: b.version.trim().trim_start_matches('v').to_string(),
                adapter_cmd: cmd,
                adapter_version: av,
                check_timeout: *x.probe_timeout.lock().unwrap(),
                after: Some(after),
            },
            Duration::from_secs(900),
        )
        .map_err(|e| {
            if e.contains("another install") {
                ApiError::Conflict(e)
            } else {
                bad(e)
            }
        })?;
    Ok(Json(json!(job)))
}

#[derive(Deserialize)]
struct UpgradeReq {
    agent: String,
    /// 版本（semver 或 dist-tag）；省略＝目錄建議版本
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    confirm: bool,
}

/// 升級／換版本：暫存目錄先裝＋實測，通過才換；用到的 lane 等回合結束後重開。
async fn upgrade(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Json(b): Json<UpgradeReq>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    if !b.confirm {
        return Err(bad("upgrading requires confirm: true"));
    }
    let e = catalog::get(&b.agent, &x.instance).ok_or(ApiError::NotFound)?;
    let spec = e
        .npm
        .clone()
        .filter(|_| e.source == "vendor")
        .ok_or_else(|| {
            bad(format!(
                "{} is not installed by AA Forum ({})",
                e.id, e.needs
            ))
        })?;
    let (name, pinned) = catalog::split_npm(&spec);
    let ver = b
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or(pinned)
        .unwrap_or("latest");
    let pkg = format!("{name}@{ver}");
    if !catalog::valid_npm(&pkg) {
        return Err(bad("invalid version"));
    }
    let legacy = catalog::LEGACY_NPM
        .iter()
        .find(|(a, p)| *a == e.id && *p != name)
        .map(|(_, p)| p.to_string());
    let npm = x.npm()?;
    let (core, chat, instance, agent, custom) = (
        s.core.clone(),
        x.chat.clone(),
        x.instance.clone(),
        e.id.clone(),
        e.custom,
    );
    let pkg2 = pkg.clone();
    let after: install::After = Box::new(move || {
        Box::pin(async move {
            if custom {
                if let Err(e) = install::set_custom_npm(&instance, &agent, &pkg2) {
                    return format!("updating custom.json failed: {e}");
                }
            }
            match chat.dispatcher(&core).restart_agent(&agent).await {
                Ok(l) if l.is_empty() => "no open sessions use this agent".into(),
                Ok(l) => format!(
                    "{} open session(s) restart with the new version after their current turn",
                    l.len()
                ),
                Err(e) => format!("restarting sessions failed: {e}"),
            }
        })
    });
    let engine_ver = match &e.engine {
        Some(en) if en.source == "cli" => {
            catalog::engine_version(en, &catalog::vendor_dir(&x.instance)).await
        }
        _ => None,
    };
    let job = x
        .jobs
        .start_upgrade(
            &x.instance,
            npm,
            install::Upgrade {
                agent: e.id.clone(),
                pkg,
                exe: e.acp.first().cloned().unwrap_or_default(),
                legacy,
                check_timeout: *x.probe_timeout.lock().unwrap(),
                engine_env: catalog::engine_env(&e.id, &x.instance),
                engine_version: engine_ver,
                after: Some(after),
            },
            Duration::from_secs(900),
        )
        .map_err(|e| {
            if e.contains("another install") {
                ApiError::Conflict(e)
            } else {
                bad(e)
            }
        })?;
    x.models_cache.lock().unwrap().remove(&b.agent);
    Ok(Json(json!(job)))
}

async fn install_status(
    Extension(x): X,
    u: User,
    Path(id): Path<String>,
    Query(q): Query<Offset>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let (j, out, off) = x.jobs.output(&id, q.offset).ok_or(ApiError::NotFound)?;
    let mut v = json!(j);
    v["output"] = json!(out);
    v["offset"] = json!(off);
    Ok(Json(v))
}

#[derive(Deserialize)]
struct AgentReq {
    agent: String,
}

fn cmd_for(x: &Inner, agent: &str) -> ApiResult<Vec<String>> {
    catalog::get(agent, &x.instance).ok_or(ApiError::NotFound)?;
    catalog::resolve_cmd(agent, &x.instance)
        .ok_or_else(|| bad(catalog::unavailable_reason(agent, &x.instance)))
}

async fn probe(Extension(x): X, u: User, Json(b): Json<AgentReq>) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let cmd = cmd_for(&x, &b.agent)?;
    let t = *x.probe_timeout.lock().unwrap();
    let r = catalog::probe(&cmd, &x.work(), t).await;
    if r["ok"] == true {
        x.models_cache
            .lock()
            .unwrap()
            .insert(b.agent.clone(), (Instant::now(), models_value(&r)));
    }
    Ok(Json(r))
}

fn models_value(probe: &Value) -> Value {
    if probe["ok"] == true {
        json!({"default": probe["model"], "models": probe["models"], "mode": probe["mode"], "modes": probe["modes"], "effort": probe["effort"], "efforts": probe["efforts"], "source": "ACP session/new"})
    } else {
        json!({"default": null, "models": [], "source": format!("could not read: {}", probe["error"].as_str().unwrap_or("").chars().take(120).collect::<String>())})
    }
}

async fn models(Extension(x): X, u: User, Path(agent): Path<String>) -> ApiResult<Json<Value>> {
    owner(&u)?;
    if let Some((t, v)) = x.models_cache.lock().unwrap().get(&agent) {
        if t.elapsed() < Duration::from_secs(1800) {
            return Ok(Json(v.clone()));
        }
    }
    let cmd = cmd_for(&x, &agent)?;
    let t = *x.probe_timeout.lock().unwrap();
    let p = catalog::probe(&cmd, &x.work(), t).await;
    let mut v = models_value(&p);
    if let Some(ms) = v["modes"].as_array() {
        let ids: Vec<&str> = ms.iter().filter_map(|m| m["id"].as_str()).collect();
        v["widest_mode"] = json!(catalog::widest_mode(&ids));
    }
    if !v["models"].as_array().is_none_or(|m| m.is_empty()) {
        x.models_cache
            .lock()
            .unwrap()
            .insert(agent, (Instant::now(), v.clone()));
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
struct NewRole {
    role: String,
    agent: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "worker")]
    rank: String,
    #[serde(default)]
    label: Option<String>,
    /// 指派的 skills
    #[serde(default)]
    skills: Vec<String>,
}

fn worker() -> String {
    "worker".into()
}

async fn add_role(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Json(b): Json<NewRole>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    if !aaf_runtime::dispatcher::valid_role_name(&b.role) {
        return Err(bad("role name: 1-32 chars of a-z 0-9 - _"));
    }
    if aaf_api::auth::user_exists(&s.core, &b.role).map_err(bad)? {
        return Err(ApiError::Conflict(format!(
            "{} is already a user name",
            b.role
        )));
    }
    cmd_for(&x, &b.agent)?;
    let mut all = roles::read(&x.instance).map_err(bad)?;
    if all.contains_key(&b.role) {
        return Err(ApiError::Conflict(format!("role {} exists", b.role)));
    }
    all.insert(
        b.role.clone(),
        roles::new_role(&b.agent, b.model.as_deref(), &b.rank, b.label.as_deref()),
    );
    roles::write(&x.instance, &all).map_err(bad)?;
    let applied = x.apply(&s.core).await?;
    let skills = if b.skills.is_empty() {
        Value::Null
    } else {
        assign_skills(&x.instance, &b.role, &b.skills).map_err(bad)?
    };
    Ok(Json(
        json!({"role": b.role, "applied": applied, "skills": skills}),
    ))
}

/// 寫 skills/_config.json 的 roles 指派（與 skills 功能同一份設定）。
fn assign_skills(
    instance: &std::path::Path,
    role: &str,
    skills: &[String],
) -> Result<Value, String> {
    let f = instance.join("skills").join("_config.json");
    let mut cfg: Value = std::fs::read_to_string(&f)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    if !cfg.is_object() {
        cfg = json!({});
    }
    for s in skills {
        if !instance.join("skills").join(s).join("SKILL.md").exists() {
            return Err(format!("skill {s} not found"));
        }
    }
    cfg["roles"][role] = json!({"skills": skills, "packs": []});
    std::fs::create_dir_all(f.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&f, serde_json::to_string_pretty(&cfg).unwrap()).map_err(|e| e.to_string())?;
    Ok(json!(skills))
}

#[derive(Deserialize)]
struct EngineReq {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    rank: Option<String>,
    #[serde(default)]
    label: Option<String>,
    /// 全域權限模式（agent 回報的 id；空字串＝清除）。YOLO 只能每群開。
    #[serde(default)]
    mode: Option<String>,
    /// 改模型前先驗（預設 true）：預設只驗連線與模型清單，不花 token
    #[serde(default = "yes")]
    check: bool,
    /// true＝改用實測（送一句「只回 OK」，花一次極小呼叫的 token）
    #[serde(default)]
    test: bool,
    /// `acp` 或 `headless`（每輪直接跑 agent 的 CLI，只限 claude／codex／pi）
    #[serde(default)]
    transport: Option<String>,
    /// 推理強度（agent 回報的 thought_level 選項）；空字串＝清除、用 agent 預設
    #[serde(default)]
    effort: Option<String>,
    /// 帳號 id（Agents 頁 → 帳號）；空字串或 default＝預設登入。只有 claude／codex
    #[serde(default)]
    account: Option<String>,
}

fn yes() -> bool {
    true
}

/// 切模型前的檢查：預設只驗連線（不花 token），test=true 才實測。
async fn verify_model(
    cmd: &[String],
    env: &[(String, String)],
    cwd: &std::path::Path,
    model: &str,
    effort: &str,
    t: std::time::Duration,
    test: bool,
) -> Value {
    if test {
        catalog::check_model_env(cmd, env, cwd, model, t).await
    } else {
        catalog::check_connect(cmd, env, cwd, model, effort, t).await
    }
}

async fn set_engine(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Path(role): Path<String>,
    Json(b): Json<EngineReq>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let mut all = roles::read(&x.instance).map_err(bad)?;
    let cur = all.get(&role).cloned().ok_or(ApiError::NotFound)?;
    let mut r = cur.clone();
    if let Some(a) = &b.agent {
        cmd_for(&x, a)?;
        r["driver"] = json!("acp");
        r["acp_agent"] = json!(a);
        if let Some(o) = r.as_object_mut() {
            o.remove("command");
            if aaf_runtime::headless::Cli::from_agent(a).is_none() {
                o.remove("transport");
            }
            if b.model.is_none() {
                o.remove("model");
            }
            if b.effort.is_none() {
                o.remove("effort");
            }
            // 帳號屬於某個 agent：換 agent 就清掉（新 agent 要用帳號請一併指定）
            o.remove("account");
        }
    }
    if let Some(a) = &b.account {
        if a.is_empty() || a == aaf_runtime::accounts::DEFAULT {
            r.as_object_mut().unwrap().remove("account");
        } else {
            r["account"] = json!(a);
        }
    }
    if let Some(e) = &b.effort {
        if e.is_empty() {
            r.as_object_mut().unwrap().remove("effort");
        } else {
            r["effort"] = json!(e);
        }
    }
    if let Some(m) = &b.model {
        if m.is_empty() {
            r.as_object_mut().unwrap().remove("model");
        } else {
            r["model"] = json!(m);
        }
    }
    if let Some(k) = &b.rank {
        r["rank"] = json!(k);
    }
    if let Some(m) = &b.mode {
        if m.is_empty() || m == catalog::WIDEST {
            r.as_object_mut().unwrap().remove("mode");
        } else {
            r["mode"] = json!(m);
        }
    }
    if let Some(l) = &b.label {
        r["label"] = json!(l);
    }
    match b.transport.as_deref() {
        None => {}
        Some("acp") | Some("") => {
            r.as_object_mut().unwrap().remove("transport");
        }
        Some("headless") => {
            let a = r["acp_agent"].as_str().unwrap_or_default();
            if aaf_runtime::headless::Cli::from_agent(a).is_none() {
                return Err(bad(format!(
                    "headless is only available for claude, codex and pi (not {a})"
                )));
            }
            r["transport"] = json!("headless");
        }
        Some(t) => return Err(bad(format!("unknown transport {t}"))),
    }
    let mut check = Value::Null;
    let agent_changed = r.get("acp_agent") != cur.get("acp_agent");
    let model_changed = r.get("model") != cur.get("model") || agent_changed;
    let effort_changed = r.get("effort") != cur.get("effort") || agent_changed;
    let model = r["model"].as_str();
    let effort = r["effort"].as_str();
    let account = r["account"].as_str().unwrap_or_default().to_string();
    let account_changed = r.get("account") != cur.get("account") || agent_changed;
    // 帳號要存在、屬於這個 agent、已登入；連線檢查也用該帳號（模型清單、推理強度可能因方案不同）
    let acct_env = match r["acp_agent"].as_str() {
        Some(a) if !account.is_empty() => accounts_api::check_assignable(&x, a, &account).await?,
        _ => vec![],
    };
    let needs = (model_changed && model.is_some())
        || (effort_changed && effort.is_some())
        || (account_changed && !account.is_empty());
    if let (true, true, Some(a)) = (b.check, needs, r["acp_agent"].as_str()) {
        let cmd = cmd_for(&x, a)?;
        let t = *x.probe_timeout.lock().unwrap();
        check = verify_model(
            &cmd,
            &acct_env,
            &x.work(),
            model.unwrap_or_default(),
            effort.unwrap_or_default(),
            t,
            b.test,
        )
        .await;
        if check["ok"] != true {
            return Err(ApiError::Unprocessable(format!(
                "model check failed; nothing changed: {}",
                check["error"].as_str().unwrap_or("?")
            )));
        }
    }
    all.insert(role.clone(), r);
    roles::write(&x.instance, &all).map_err(bad)?;
    let applied = x.apply(&s.core).await?;
    Ok(Json(
        json!({"role": role, "applied": applied, "check": check}),
    ))
}

/// 每群覆寫：疊加在全域上，只寫有變的欄位。
#[derive(Deserialize, Default)]
struct OverrideReq {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    /// 空＝跟角色；default＝本群用預設登入；其他＝帳號 id
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    yolo: bool,
    #[serde(default)]
    yolo_until: Option<String>,
    #[serde(default = "yes")]
    check: bool,
    #[serde(default)]
    test: bool,
}

fn room_role(s: &AppState, room: i64, role: &str) -> ApiResult<()> {
    let ok = s
        .core
        .db
        .with(|c| aaf_feature_chat::store::is_member(c, room, role))
        .map_err(bad)?;
    if !ok {
        return Err(bad(format!("{role} is not in room {room}")));
    }
    Ok(())
}

/// 生效值：全域＋本群覆寫。
fn effective(file: &Value, ov: &Value) -> Value {
    let agent = ov["acp_agent"].as_str().or(file["acp_agent"].as_str());
    let same_agent = ov["acp_agent"].is_null() || ov["acp_agent"] == file["acp_agent"];
    let model = ov["model"].as_str().or(if same_agent {
        file["model"].as_str()
    } else {
        None
    });
    let mode = ov["mode"].as_str().or(if same_agent {
        file["mode"].as_str()
    } else {
        None
    });
    let effort = ov["effort"].as_str().or(if same_agent {
        file["effort"].as_str()
    } else {
        None
    });
    let account = ov["account"].as_str().or(if same_agent {
        file["account"].as_str()
    } else {
        None
    });
    json!({"agent": agent, "model": model, "mode": mode, "effort": effort, "account": account})
}

async fn room_override_get(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Path((room, role)): Path<(i64, String)>,
) -> ApiResult<Json<Value>> {
    if u.is_role() {
        return Err(ApiError::Forbidden("people only".into()));
    }
    let owner_view = u.rank == Rank::Owner;
    if !owner_view {
        let ok = s
            .core
            .db
            .with(|c| aaf_feature_chat::store::is_member(c, room, &u.name))
            .map_err(bad)?;
        if !ok {
            return Err(ApiError::NotFound);
        }
    }
    let all = roles::read(&x.instance).map_err(bad)?;
    let file = all.get(&role).cloned().ok_or(ApiError::NotFound)?;
    let ov = file["room_overrides"][room.to_string()].clone();
    let today = aaf_runtime::dispatcher::today();
    let yolo_on = ov["yolo"] == true
        && ov["yolo_until"]
            .as_str()
            .is_none_or(|d| today.as_str() <= d);
    Ok(Json(json!({
        "role": role, "room": room,
        "global": {"agent": file["acp_agent"], "model": file["model"], "mode": file["mode"], "effort": file["effort"], "account": file["account"]},
        "override": ov,
        "effective": effective(&file, &ov),
        "yolo": yolo_on,
        "can_edit": owner_view,
    })))
}

fn notice(s: &AppState, x: &Inner, room: i64, body: &str) -> ApiResult<()> {
    let roster = x.chat.roster();
    aaf_feature_chat::store::write(&s.core, |tx, evs| {
        aaf_feature_chat::store::post_system(tx, evs, &roster, room, body)?;
        Ok(())
    })
    .map_err(bad)
}

async fn room_override_put(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Path((room, role)): Path<(i64, String)>,
    Json(b): Json<OverrideReq>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    room_role(&s, room, &role)?;
    if let Some(d) = b.yolo_until.as_deref().filter(|d| !d.is_empty()) {
        if jiff::civil::Date::strptime("%Y-%m-%d", d).is_err() {
            return Err(bad("yolo_until must be YYYY-MM-DD"));
        }
    }
    let mut all = roles::read(&x.instance).map_err(bad)?;
    let mut file = all.get(&role).cloned().ok_or(ApiError::NotFound)?;
    if file["driver"] != "acp" {
        return Err(bad("only ACP roles can be overridden per room"));
    }
    let mut ov = serde_json::Map::new();
    let nonempty = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(a) = nonempty(&b.agent).filter(|a| file["acp_agent"].as_str() != Some(a)) {
        cmd_for(&x, &a)?;
        ov.insert("acp_agent".into(), json!(a));
    }
    if let Some(m) = nonempty(&b.model) {
        ov.insert("model".into(), json!(m));
    }
    if let Some(m) = nonempty(&b.mode) {
        ov.insert("mode".into(), json!(m));
    }
    if let Some(e) = nonempty(&b.effort) {
        ov.insert("effort".into(), json!(e));
    }
    if let Some(a) = nonempty(&b.account) {
        ov.insert("account".into(), json!(a));
    }
    if b.yolo {
        ov.insert("yolo".into(), json!(true));
        if let Some(d) = nonempty(&b.yolo_until) {
            ov.insert("yolo_until".into(), json!(d));
        }
    }
    let ov = Value::Object(ov);
    let old = file["room_overrides"][room.to_string()].clone();
    let eff = effective(&file, &ov);
    let old_eff = effective(&file, &old);
    let mut check = Value::Null;
    let agent_changed = eff["agent"] != old_eff["agent"];
    let model_moved = eff["model"] != old_eff["model"] || agent_changed;
    let effort_moved = eff["effort"] != old_eff["effort"] || agent_changed;
    let account_moved = eff["account"] != old_eff["account"] || agent_changed;
    let eff_account = eff["account"].as_str().unwrap_or_default();
    let acct_env = match eff["agent"].as_str() {
        Some(a) if !eff_account.is_empty() => {
            accounts_api::check_assignable(&x, a, eff_account).await?
        }
        _ => vec![],
    };
    if b.check
        && ((model_moved && !eff["model"].is_null())
            || (effort_moved && !eff["effort"].is_null())
            || (account_moved && !eff_account.is_empty()))
    {
        if let Some(a) = eff["agent"].as_str() {
            let cmd = cmd_for(&x, a)?;
            let t = *x.probe_timeout.lock().unwrap();
            check = verify_model(
                &cmd,
                &acct_env,
                &x.work(),
                eff["model"].as_str().unwrap_or_default(),
                eff["effort"].as_str().unwrap_or_default(),
                t,
                b.test,
            )
            .await;
            if check["ok"] != true {
                return Err(ApiError::Unprocessable(format!(
                    "model check failed; nothing changed: {}",
                    check["error"].as_str().unwrap_or("?")
                )));
            }
        }
    }
    let obj = file.as_object_mut().unwrap();
    let ros = obj.entry("room_overrides").or_insert_with(|| json!({}));
    if ov.as_object().is_some_and(|o| o.is_empty()) {
        ros.as_object_mut().map(|m| m.remove(&room.to_string()));
    } else {
        ros[room.to_string()] = ov.clone();
    }
    if ros.as_object().is_some_and(|m| m.is_empty()) {
        obj.remove("room_overrides");
    }
    all.insert(role.clone(), file);
    roles::write(&x.instance, &all).map_err(bad)?;
    let applied = x.apply(&s.core).await?;
    if ov != old {
        let what = format!(
            "{} · {}{}{}",
            eff["agent"].as_str().unwrap_or("?"),
            eff["model"].as_str().unwrap_or("default model"),
            eff["effort"]
                .as_str()
                .map(|e| format!(" · effort {e}"))
                .unwrap_or_default(),
            eff["account"]
                .as_str()
                .map(|a| format!(" · account {a}"))
                .unwrap_or_default()
        );
        let perm = if b.yolo {
            match nonempty(&b.yolo_until) {
                Some(d) => format!("YOLO until {d}"),
                None => "YOLO".into(),
            }
        } else {
            eff["mode"].as_str().unwrap_or("default").to_string()
        };
        notice(
            &s,
            &x,
            room,
            &format!(
                "{} set {role} in this room: {what}, permissions: {perm}.",
                u.name
            ),
        )?;
    }
    Ok(Json(
        json!({"role": role, "room": room, "override": ov, "applied": applied, "check": check}),
    ))
}

async fn room_override_delete(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Path((room, role)): Path<(i64, String)>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let mut all = roles::read(&x.instance).map_err(bad)?;
    let mut file = all.get(&role).cloned().ok_or(ApiError::NotFound)?;
    let removed = file
        .get_mut("room_overrides")
        .and_then(|r| r.as_object_mut())
        .and_then(|m| m.remove(&room.to_string()))
        .is_some();
    if file["room_overrides"]
        .as_object()
        .is_some_and(|m| m.is_empty())
    {
        file.as_object_mut().unwrap().remove("room_overrides");
    }
    if removed {
        all.insert(role.clone(), file);
        roles::write(&x.instance, &all).map_err(bad)?;
        x.apply(&s.core).await?;
        notice(
            &s,
            &x,
            room,
            &format!(
                "{} reset {role} in this room to its global settings.",
                u.name
            ),
        )?;
    }
    Ok(Json(
        json!({"role": role, "room": room, "removed": removed}),
    ))
}

/// 群被永久刪除時清掉各角色在該群的覆寫。
pub struct OverrideCleaner(pub Arc<Inner>);

#[async_trait::async_trait]
impl aaf_core::feature::Subscriber for OverrideCleaner {
    fn name(&self) -> &'static str {
        "agents_override_cleaner"
    }
    fn kinds(&self) -> &'static [&'static str] {
        &["chat.room.purged"]
    }
    async fn handle(
        &self,
        core: &Arc<Core>,
        ev: &aaf_contract::EventEnvelope,
    ) -> aaf_core::Result<()> {
        let Some(room) = ev.payload["room"]
            .as_i64()
            .or(ev.payload["room"]["id"].as_i64())
        else {
            return Ok(());
        };
        let Ok(mut all) = roles::read(&self.0.instance) else {
            return Ok(());
        };
        let mut changed = false;
        for v in all.values_mut() {
            if let Some(m) = v.get_mut("room_overrides").and_then(|r| r.as_object_mut()) {
                changed |= m.remove(&room.to_string()).is_some();
                if m.is_empty() {
                    v.as_object_mut().unwrap().remove("room_overrides");
                }
            }
        }
        if changed && roles::write(&self.0.instance, &all).is_ok() {
            let _ = self.0.chat.reload(core).await;
        }
        Ok(())
    }
}

async fn remove_role(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Path(role): Path<String>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let mut all = roles::read(&x.instance).map_err(bad)?;
    all.remove(&role).ok_or(ApiError::NotFound)?;
    roles::write(&x.instance, &all).map_err(bad)?;
    let applied = x.apply(&s.core).await?;
    Ok(Json(json!({"removed": role, "applied": applied})))
}

async fn quota_all(State(s): State<AppState>, Extension(x): X, u: User) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let mut cache = x.quota_cache.lock().await;
    if let Some((t, v)) = cache.as_ref() {
        if t.elapsed() < Duration::from_secs(60) {
            return Ok(Json(v.clone()));
        }
    }
    let installed: Vec<(String, String)> = catalog::all(&x.instance)
        .into_iter()
        .filter(|e| catalog::resolve_cmd(&e.id, &x.instance).is_some())
        .map(|e| (e.id, e.label))
        .collect();
    let in_use: Vec<String> = roles::read(&x.instance)
        .unwrap_or_default()
        .values()
        .filter_map(|r| r["acp_agent"].as_str().map(String::from))
        .collect();
    let mut accounts = quota::all(&installed, &in_use).await;
    for a in accounts.iter_mut().filter(|a| a["agent"] == "claude") {
        accounts_api::enrich_claude(&s.core, a, aaf_runtime::accounts::DEFAULT);
    }
    let v = json!({"accounts": accounts});
    *cache = Some((Instant::now(), v.clone()));
    Ok(Json(v))
}

#[derive(Deserialize, Default)]
struct RefreshBody {
    /// 空或 default＝預設登入
    #[serde(default)]
    account: Option<String>,
}

/// 手動重新整理 Claude 額度：送一個一個字的回合（haiku），把回合附帶的額度記成事件。
async fn quota_refresh(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Json(b): Json<RefreshBody>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let account = b
        .account
        .filter(|a| !a.is_empty() && a != aaf_runtime::accounts::DEFAULT);
    let env = aaf_runtime::accounts::env_for(&x.instance, Some("claude"), account.as_deref())
        .map_err(bad)?;
    let info = quota::claude_probe(&env)
        .await
        .map_err(|e| ApiError::Unprocessable(format!("could not read Claude quota: {e}")))?;
    let id = account.as_deref().unwrap_or(aaf_runtime::accounts::DEFAULT);
    let v = aaf_runtime::ratelimit::normalize(&info, id, aaf_runtime::ratelimit::now_secs())
        .ok_or_else(|| ApiError::Unprocessable("the turn reported no usage windows".into()))?;
    s.core
        .bus
        .publish("runtime.agent.ratelimit", 1, v.clone())
        .map_err(bad)?;
    *x.quota_cache.lock().await = None;
    Ok(Json(v))
}

/// 每個角色：設定、lane 狀態、最近回合／失敗。
/// driver 用目錄 agent（沒給 command）且解析不到時的原因；與 driver 啟動時的判斷一致。
fn agent_error(
    spec: &aaf_runtime::driver::DriverSpec,
    instance: &std::path::Path,
) -> Option<String> {
    if let Some(cli) = spec.headless() {
        return catalog::which(cli.program())
            .is_none()
            .then(|| format!("headless needs the `{}` CLI on PATH", cli.program()));
    }
    match spec {
        aaf_runtime::driver::DriverSpec::Acp {
            command,
            acp_agent: Some(a),
            ..
        } if command.is_empty() && catalog::resolve_cmd(a, instance).is_none() => {
            Some(catalog::unavailable_reason(a, instance))
        }
        _ => None,
    }
}

async fn health(State(s): State<AppState>, Extension(x): X, u: User) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let roster = x.chat.roster();
    let file = roles::read(&x.instance).unwrap_or_default();
    let lanes = x.chat.dispatcher(&s.core).lane_states().await;
    let mut out = vec![];
    let mut names: Vec<&String> = roster.roles.keys().collect();
    names.sort();
    for name in names {
        let r = &roster.roles[name];
        let f = file.get(name).cloned().unwrap_or(Value::Null);
        let ls: Vec<Value> = lanes
            .iter()
            .filter(|(l, _)| &l.role == name)
            .map(|(l, st)| json!({"room": l.room, "status": format!("{st:?}").to_lowercase()}))
            .collect();
        let last = turns::last_for(&s.core, name).map_err(bad)?;
        let today = aaf_runtime::dispatcher::today();
        // 每群生效的 driver 也檢查：群覆寫指到已移除／未安裝的 agent 時，卡片同樣給原因
        let overrides: Vec<Value> = r
            .room_overrides
            .iter()
            .map(|(room, o)| {
                json!({"room": room, "yolo": o.yolo_on(&today),
            "agent_error": agent_error(&o.apply(&r.driver, &today), &x.instance)})
            })
            .collect();
        // 全域 agent 解析不到（未安裝或已從目錄移除）：卡片直接顯示原因，不等第一次被 @ 才在 log 裡出錯
        let agent_error = agent_error(&r.driver, &x.instance);
        out.push(json!({"role": name, "rank": r.rank, "label": f["label"], "agent": f["acp_agent"], "model": f["model"], "mode": f["mode"], "effort": f["effort"], "account": f["account"], "transport": f["transport"], "room_overrides": overrides,
            "driver": f["driver"], "workdir": r.workdir, "lanes": ls, "last_turn": last, "agent_error": agent_error}));
    }
    Ok(Json(json!({"roles": out})))
}

async fn dashboard(State(s): State<AppState>, Extension(x): X, u: User) -> ApiResult<Json<Value>> {
    if u.is_role() {
        return Err(ApiError::Forbidden("people only".into()));
    }
    let roster = x.chat.roster();
    let lanes = x.chat.dispatcher(&s.core).lane_states().await;
    let working = lanes
        .iter()
        .filter(|(_, st)| *st == aaf_runtime::AgentStatus::Working)
        .count();
    let owner = u.rank == Rank::Owner;
    let rooms =
        aaf_feature_chat::store::visible_room_ids(&s.core.db, &u.name, owner).map_err(bad)?;
    let recent = turns::recent(&s.core, &rooms, 30).map_err(bad)?;
    let stats = turns::stats(&s.core, &rooms, 24 * 3600).map_err(bad)?;
    let unread = aaf_feature_chat::store::queued_by_room(&s.core.db).map_err(bad)?;
    let unread: Vec<Value> = unread
        .into_iter()
        .filter(|(r, _, _)| rooms.contains(r))
        .map(|(r, name, n)| json!({"room": r, "name": name, "queued": n}))
        .collect();
    let alerts = turns::alerts(&s.core, &rooms, 24 * 3600).map_err(bad)?;
    Ok(Json(json!({
        "roles": roster.roles.len(),
        "lanes_open": lanes.len(),
        "working": working,
        "rooms": rooms.len(),
        "queued": unread,
        "turns_24h": stats,
        "recent": recent,
        "alerts": alerts,
    })))
}

pub(crate) fn router(inner: Arc<Inner>) -> Router<AppState> {
    Router::new()
        .route("/catalog", get(catalog_list))
        .route("/plan-custom", post(plan_custom))
        .route("/install", post(install_start))
        .route("/install/{id}", get(install_status))
        .route("/packages", get(packages))
        .route("/packages/check", post(packages_check))
        .route("/packages/upgrade", post(upgrade))
        .route("/packages/engine-upgrade", post(engine_upgrade))
        .route("/packages/test", post(compat_test))
        .route("/probe", post(probe))
        .route("/models/{agent}", get(models))
        .route("/roles", post(add_role))
        .route("/roles/{role}/engine", post(set_engine))
        .route("/roles/{role}", delete(remove_role))
        .route(
            "/rooms/{room}/roles/{role}",
            get(room_override_get)
                .put(room_override_put)
                .delete(room_override_delete),
        )
        .route(
            "/accounts",
            get(accounts_api::list).post(accounts_api::create),
        )
        .route("/accounts/{id}", delete(accounts_api::remove))
        .route("/quota", get(quota_all))
        .route("/quota/refresh", post(quota_refresh))
        .route("/health", get(health))
        .route("/dashboard", get(dashboard))
        .layer(Extension(inner))
}

impl Feature for Agents {
    fn name(&self) -> &'static str {
        "agents"
    }
    fn migrations(&self) -> &'static [Migration] {
        turns::MIGRATIONS
    }
    fn subscribers(&self) -> Vec<Arc<dyn aaf_core::feature::Subscriber>> {
        vec![
            Arc::new(turns::Recorder),
            Arc::new(OverrideCleaner(self.inner.clone())),
        ]
    }
}

impl HttpFeature for Agents {
    fn name(&self) -> &'static str {
        "agents"
    }
    fn routes(&self) -> Router<AppState> {
        router(self.inner.clone())
    }
    fn nav(&self) -> Vec<(&'static str, &'static str)> {
        vec![("Dashboard", "/dashboard"), ("Agents", "/agents")]
    }
}
