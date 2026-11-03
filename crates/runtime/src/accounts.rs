//! 多帳號（Claude／Codex）：每個帳號是一個獨立的設定資料夾，啟動 agent 時用環境變數指過去
//! （Claude 的 `CLAUDE_CONFIG_DIR`、Codex 的 `CODEX_HOME`）。aaf 不讀、不複製、不搬動任何憑證；
//! 登入由使用者用該 agent 自己的登入指令完成。帳號清單存在 `deploy/accounts.json`。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 內建的「預設帳號」：不設環境變數，agent 用它平常的登入。
pub const DEFAULT: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// claude 或 codex
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 這個帳號的設定資料夾（絕對路徑）
    pub dir: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    accounts: BTreeMap<String, Account>,
}

pub fn path(instance: &Path) -> PathBuf {
    instance.join("deploy").join("accounts.json")
}

/// 帳號放環境變數的名字；沒有多帳號機制的 agent 回 None。
pub fn home_var(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        _ => None,
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id != DEFAULT
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
}

pub fn load(instance: &Path) -> Result<BTreeMap<String, Account>, String> {
    let p = path(instance);
    match std::fs::read_to_string(&p) {
        Ok(t) => serde_json::from_str::<File>(&t)
            .map(|f| f.accounts)
            .map_err(|e| format!("accounts.json: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

/// 原子寫入（暫存檔＋rename）。
pub fn save(instance: &Path, accounts: &BTreeMap<String, Account>) -> Result<(), String> {
    let p = path(instance);
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&File {
        accounts: accounts.clone(),
    })
    .map_err(|e| e.to_string())?;
    let tmp = p.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, text + "\n").map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// 啟動某角色／某群時要加的環境變數。`account` 空或 default＝不加（用預設登入）。
/// 帳號不存在、或屬於別的 agent，回錯誤（讓該 lane 明確啟動失敗，而不是悄悄用到別的帳號）。
pub fn env_for(
    instance: &Path,
    agent: Option<&str>,
    account: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let Some(id) = account.filter(|a| !a.is_empty() && *a != DEFAULT) else {
        return Ok(vec![]);
    };
    let all = load(instance)?;
    let a = all
        .get(id)
        .ok_or_else(|| format!("account {id} not found (Agents page → Accounts)"))?;
    if agent.is_some_and(|ag| ag != a.agent) {
        return Err(format!(
            "account {id} is for {}, not {}",
            a.agent,
            agent.unwrap_or("?")
        ));
    }
    let var =
        home_var(&a.agent).ok_or_else(|| format!("{} has no multi-account support", a.agent))?;
    Ok(vec![(var.to_string(), a.dir.display().to_string())])
}

/// 登入指令（給使用者在終端機執行；aaf 不代為輸入憑證）。
pub fn login_command(agent: &str, dir: Option<&Path>) -> String {
    let prefix = match (home_var(agent), dir) {
        (Some(v), Some(d)) => format!("{v}='{}' ", d.display()),
        _ => String::new(),
    };
    match agent {
        "claude" => format!("{prefix}claude auth login"),
        "codex" => format!("{prefix}codex login"),
        other => format!("{prefix}{other}"),
    }
}

/// 檢查登入狀態用的指令（唯讀、不開回合、不耗額度）。
pub fn status_command(agent: &str) -> Option<(&'static str, &'static [&'static str])> {
    match agent {
        "claude" => Some(("claude", &["auth", "status"])),
        "codex" => Some(("codex", &["login", "status"])),
        _ => None,
    }
}

/// 建立帳號資料夾（0700），並把**非憑證**的設定複製過去，讓新帳號的行為跟預設一致：
/// Claude：settings.json、CLAUDE.md；Codex：config.toml。不複製歷史、登入資料或帳號識別檔。
pub fn prepare_dir(agent: &str, dir: &Path, copy_settings: bool) -> Result<Vec<String>, String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let mut copied = vec![];
    if !copy_settings {
        return Ok(copied);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (src, files): (Option<PathBuf>, &[&str]) = match agent {
        "claude" => (
            home.as_ref().map(|h| h.join(".claude")),
            &["settings.json", "CLAUDE.md"],
        ),
        "codex" => (
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(".codex"))),
            &["config.toml"],
        ),
        _ => (None, &[]),
    };
    if let Some(src) = src {
        for f in files {
            let (from, to) = (src.join(f), dir.join(f));
            if from.is_file() && !to.exists() && std::fs::copy(&from, &to).is_ok() {
                copied.push((*f).to_string());
            }
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_env() {
        assert!(valid_id("work") && valid_id("a-1_b"));
        assert!(!valid_id("") && !valid_id("default") && !valid_id("Work") && !valid_id("-x"));
        let d = tempfile::tempdir().unwrap();
        assert_eq!(env_for(d.path(), Some("claude"), None).unwrap(), vec![]);
        assert_eq!(
            env_for(d.path(), Some("claude"), Some("default")).unwrap(),
            vec![]
        );
        assert!(env_for(d.path(), Some("claude"), Some("nope"))
            .unwrap_err()
            .contains("not found"));
        let mut m = BTreeMap::new();
        m.insert(
            "w".to_string(),
            Account {
                agent: "claude".into(),
                label: None,
                dir: "/x/w".into(),
            },
        );
        save(d.path(), &m).unwrap();
        assert_eq!(load(d.path()).unwrap(), m);
        assert_eq!(
            env_for(d.path(), Some("claude"), Some("w")).unwrap(),
            vec![("CLAUDE_CONFIG_DIR".to_string(), "/x/w".to_string())]
        );
        assert!(env_for(d.path(), Some("codex"), Some("w"))
            .unwrap_err()
            .contains("is for claude"));
        assert_eq!(
            login_command("codex", Some(Path::new("/x/c"))),
            "CODEX_HOME='/x/c' codex login"
        );
    }

    #[test]
    fn prepare_copies_only_non_credential_settings() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("acct");
        assert!(prepare_dir("claude", &dir, false).unwrap().is_empty());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
