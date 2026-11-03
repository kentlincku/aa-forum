//! 多帳號 API（只有 owner）：列出（登入狀態、方案、額度）、新增、刪除，以及指給角色／群時的檢查。
//! 不讀、不複製、不回傳任何憑證；登入由使用者在終端機用 agent 自己的指令完成。

use super::*;
use aaf_runtime::accounts::{self, Account};
use std::collections::BTreeMap;
use std::path::Path as FsPath;

/// 「a***@example.com」：認得出是哪個帳號，但不回傳完整信箱。
fn mask_email(e: &str) -> String {
    match e.split_once('@') {
        Some((l, d)) if !l.is_empty() => format!("{}***@{d}", l.chars().next().unwrap()),
        _ => "***".into(),
    }
}

async fn capture(
    exe: &str,
    args: &[&str],
    env: &[(String, String)],
) -> Result<(i32, String), String> {
    let path = catalog::which(exe).ok_or_else(|| format!("{exe} not found"))?;
    let o = tokio::time::timeout(
        Duration::from_secs(12),
        tokio::process::Command::new(path)
            .args(args)
            .envs(env.iter().cloned())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| format!("{exe} timed out"))?
    .map_err(|e| format!("{exe}: {e}"))?;
    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&o.stderr));
    Ok((o.status.code().unwrap_or(-1), text))
}

/// 登入狀態：{logged_in: bool|null, method?, plan?, email?(已遮)}。null＝查不出來。
pub async fn status(agent: &str, env: &[(String, String)]) -> Value {
    let Some((exe, args)) = accounts::status_command(agent) else {
        return json!({"logged_in": null, "detail": "no status command for this agent"});
    };
    let (_, text) = match capture(exe, args, env).await {
        Ok(r) => r,
        Err(e) => return json!({"logged_in": null, "detail": e}),
    };
    match agent {
        "claude" => match serde_json::from_str::<Value>(text.trim()) {
            Ok(d) if d["loggedIn"].is_boolean() => json!({
                "logged_in": d["loggedIn"],
                "method": d["authMethod"],
                "plan": d["subscriptionType"],
                "email": d["email"].as_str().map(mask_email),
            }),
            _ => json!({"logged_in": null, "detail": "unexpected output from claude auth status"}),
        },
        _ => {
            let first = text
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            let l = first.to_lowercase();
            if l.starts_with("logged in") {
                json!({"logged_in": true, "method": first})
            } else if l.contains("not logged in") {
                json!({"logged_in": false})
            } else {
                json!({"logged_in": null, "detail": first})
            }
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn default_home(agent: &str) -> Option<PathBuf> {
    match agent {
        "claude" => home().map(|h| h.join(".claude")),
        "codex" => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".codex"))),
        _ => None,
    }
}

/// 哪些角色／群在用哪個帳號：account id → ["planner", "maker@8", …]。沒指定帳號的 claude／codex 角色算預設帳號。
fn usage(roles: &serde_json::Map<String, Value>) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, r) in roles {
        let agent = r["acp_agent"].as_str().unwrap_or_default();
        if accounts::home_var(agent).is_none() {
            continue;
        }
        let acct = r["account"]
            .as_str()
            .filter(|a| !a.is_empty())
            .unwrap_or(accounts::DEFAULT);
        out.entry(format!("{agent}:{acct}"))
            .or_default()
            .push(name.clone());
        for (room, ov) in r["room_overrides"].as_object().into_iter().flatten() {
            let oa = ov["acp_agent"].as_str().unwrap_or(agent);
            if let Some(a) = ov["account"].as_str().filter(|a| !a.is_empty()) {
                out.entry(format!("{oa}:{a}"))
                    .or_default()
                    .push(format!("{name}@{room}"));
            }
        }
    }
    out
}

/// `/usage` 沒給視窗（token 登入）時，改用最近一個回合附帶的額度（`runtime.agent.ratelimit` 事件）。
pub fn enrich_claude(core: &aaf_core::Core, q: &mut Value, account: &str) {
    if q["windows"].as_array().is_some_and(|w| !w.is_empty()) {
        return;
    }
    let row: Option<String> = core
        .db
        .with(|c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT payload FROM events WHERE kind = 'runtime.agent.ratelimit' \
                 AND json_extract(payload, '$.account') = ?1 ORDER BY seq DESC LIMIT 1",
                [account],
                |r| r.get(0),
            )
            .optional()?)
        })
        .ok()
        .flatten();
    let Some(p) = row.and_then(|p| serde_json::from_str::<Value>(&p).ok()) else {
        return;
    };
    q["windows"] = p["windows"].clone();
    q["observed_at"] = p["observed_at"].clone();
    q["source"] = json!("last turn (rate limit reported with each turn)");
    q["from_turn"] = json!(true);
    if let Some(o) = q.as_object_mut() {
        o.remove("note");
    }
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    /// 只要清單（不查登入狀態和額度）：選單用，很快
    #[serde(default)]
    light: bool,
}

pub async fn list(
    State(s): State<AppState>,
    Extension(x): X,
    u: User,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let light = q.light;
    let stored = accounts::load(&x.instance).map_err(bad)?;
    let used = usage(&roles::read(&x.instance).map_err(bad)?);
    // 預設帳號：只列有安裝的 agent
    let mut rows: Vec<(String, String, Option<Account>)> = vec![];
    for agent in ["claude", "codex"] {
        if catalog::which(agent).is_some() {
            rows.push((accounts::DEFAULT.into(), agent.into(), None));
        }
    }
    for (id, a) in &stored {
        rows.push((id.clone(), a.agent.clone(), Some(a.clone())));
    }
    let mut tasks = vec![];
    for (id, agent, acct) in rows {
        let env = acct
            .as_ref()
            .and_then(|a| {
                accounts::home_var(&a.agent)
                    .map(|v| vec![(v.to_string(), a.dir.display().to_string())])
            })
            .unwrap_or_default();
        let users = used
            .get(&format!("{agent}:{id}"))
            .cloned()
            .unwrap_or_default();
        let core = s.core.clone();
        tasks.push(tokio::spawn(async move {
            let st = if light {
                Value::Null
            } else {
                status(&agent, &env).await
            };
            let quota = match (agent.as_str(), st["logged_in"].as_bool()) {
                ("codex", Some(true)) => quota::codex_with(&env).await,
                ("claude", Some(true)) => {
                    let mut q = quota::claude_with(&env).await;
                    enrich_claude(&core, &mut q, &id);
                    q
                }
                _ => Value::Null,
            };
            let dir = acct.as_ref().map(|a| a.dir.clone());
            json!({
                "id": id, "agent": agent, "default": acct.is_none(),
                "label": acct.as_ref().and_then(|a| a.label.clone()),
                "dir": dir.as_ref().map(|d| d.display().to_string()),
                "login_command": accounts::login_command(&agent, dir.as_deref()),
                "status": st, "quota": quota, "used_by": users,
            })
        }));
    }
    let mut out = vec![];
    for t in tasks {
        if let Ok(v) = t.await {
            out.push(v);
        }
    }
    Ok(Json(
        json!({"accounts": out, "root": x.instance.join("accounts")}),
    ))
}

#[derive(Deserialize)]
pub struct NewAccount {
    id: String,
    agent: String,
    #[serde(default)]
    label: Option<String>,
    /// 省略＝`<instance>/accounts/<agent>-<id>`
    #[serde(default)]
    dir: Option<String>,
    /// 把非憑證的設定（Claude 的 settings.json／CLAUDE.md、Codex 的 config.toml）複製過去
    #[serde(default = "yes")]
    copy_settings: bool,
}

pub async fn create(Extension(x): X, u: User, Json(b): Json<NewAccount>) -> ApiResult<Json<Value>> {
    owner(&u)?;
    if !accounts::valid_id(&b.id) {
        return Err(bad(
            "id must be 1-32 chars of a-z, 0-9, - or _ (and not 'default')",
        ));
    }
    let Some((exe, _)) = accounts::status_command(&b.agent) else {
        return Err(bad("accounts are only available for claude and codex"));
    };
    if catalog::which(exe).is_none() {
        return Err(bad(format!("{exe} is not installed")));
    }
    let mut all = accounts::load(&x.instance).map_err(bad)?;
    if all.contains_key(&b.id) {
        return Err(ApiError::Conflict(format!(
            "account {} already exists",
            b.id
        )));
    }
    let dir = match b.dir.as_deref().filter(|d| !d.trim().is_empty()) {
        None => x
            .instance
            .join("accounts")
            .join(format!("{}-{}", b.agent, b.id)),
        Some(d) => {
            let d = match (d.strip_prefix("~/"), home()) {
                (Some(rest), Some(h)) => h.join(rest),
                _ => PathBuf::from(d),
            };
            if !d.is_absolute()
                || d.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(bad("dir must be an absolute path without .."));
            }
            d
        }
    };
    if default_home(&b.agent).is_some_and(|h| h == dir) {
        return Err(bad(
            "that is the default login's folder; use the built-in default account",
        ));
    }
    if all.values().any(|a| a.dir == dir) {
        return Err(ApiError::Conflict(
            "another account already uses that folder".into(),
        ));
    }
    let copied = accounts::prepare_dir(&b.agent, &dir, b.copy_settings).map_err(bad)?;
    let label = b.label.filter(|l| !l.trim().is_empty());
    all.insert(
        b.id.clone(),
        Account {
            agent: b.agent.clone(),
            label,
            dir: dir.clone(),
        },
    );
    accounts::save(&x.instance, &all).map_err(bad)?;
    Ok(Json(json!({
        "id": b.id, "agent": b.agent, "dir": dir,
        "login_command": accounts::login_command(&b.agent, Some(&dir)),
        "copied_settings": copied,
        "next": "run the login command in a terminal, then assign the account to a role",
    })))
}

pub async fn remove(Extension(x): X, u: User, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    owner(&u)?;
    let mut all = accounts::load(&x.instance).map_err(bad)?;
    let a = all.get(&id).cloned().ok_or(ApiError::NotFound)?;
    let used = usage(&roles::read(&x.instance).map_err(bad)?)
        .remove(&format!("{}:{id}", a.agent))
        .unwrap_or_default();
    if !used.is_empty() {
        return Err(ApiError::Conflict(format!(
            "account {id} is used by {}; switch them to another account first",
            used.join(", ")
        )));
    }
    all.remove(&id);
    accounts::save(&x.instance, &all).map_err(bad)?;
    // 資料夾（含登入資料）不刪：要清掉請自己處理
    Ok(Json(json!({"removed": id, "dir_kept": a.dir})))
}

/// 指給角色／群之前的檢查：帳號存在、屬於這個 agent、而且已經登入。回傳要加的環境變數（給連線檢查用）。
pub async fn check_assignable(
    x: &Inner,
    agent: &str,
    account: &str,
) -> ApiResult<Vec<(String, String)>> {
    if account.is_empty() || account == accounts::DEFAULT {
        return Ok(vec![]);
    }
    let env = accounts::env_for(&x.instance, Some(agent), Some(account)).map_err(bad)?;
    let all = accounts::load(&x.instance).map_err(bad)?;
    let dir = all.get(account).map(|a| a.dir.clone());
    if status(agent, &env).await["logged_in"] == false {
        return Err(ApiError::Unprocessable(format!(
            "account {account} is not logged in yet; run in a terminal: {}",
            accounts::login_command(agent, dir.as_deref().map(FsPath::new))
        )));
    }
    Ok(env)
}
