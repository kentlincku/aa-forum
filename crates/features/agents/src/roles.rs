//! roles.json 讀寫（原子寫入、保留未知欄位）。套用交給 chat 的熱重載。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub fn path(instance: &Path) -> PathBuf {
    instance.join("deploy").join("roles.json")
}

pub fn read(instance: &Path) -> Result<Map<String, Value>, String> {
    let p = path(instance);
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("roles.json: {e}"))?;
    Ok(v["roles"].as_object().cloned().unwrap_or_default())
}

/// 原子寫入（暫存檔＋rename）；寫之前用 runtime 的解析器驗證，不合法就不寫。
pub fn write(instance: &Path, roles: &Map<String, Value>) -> Result<(), String> {
    let p = path(instance);
    let text = serde_json::to_string_pretty(&json!({"roles": roles})).map_err(|e| e.to_string())?;
    aaf_runtime::dispatcher::Roster::from_json(&text, instance).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = p.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, text + "\n").map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// 新角色的 roles.json 內容。
pub fn new_role(agent: &str, model: Option<&str>, rank: &str, label: Option<&str>) -> Value {
    let mut r = json!({"driver": "acp", "acp_agent": agent, "rank": rank});
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        r["model"] = json!(m);
    }
    if let Some(l) = label.filter(|l| !l.is_empty()) {
        r["label"] = json!(l);
    }
    r
}
