//! 轉接器＋本體的相容紀錄。
//! 每次實測（升級轉接器、升級本體、手動「測試相容」）記一筆；卡片顯示「目前這一組」的結果。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn file(instance: &Path) -> PathBuf {
    instance.join("var").join("acp-compat.json")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read(instance: &Path) -> Vec<Value> {
    std::fs::read_to_string(file(instance))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// 記一筆實測結果。`check` 是 catalog::check_model 的回傳（ok／model／secs／error）。
pub fn record(
    instance: &Path,
    agent: &str,
    adapter: Option<&str>,
    engine: Option<&str>,
    check: &Value,
    how: &str,
) {
    let _g = LOCK.lock().unwrap();
    let mut all = read(instance);
    all.push(json!({
        "agent": agent, "adapter": adapter, "engine": engine, "ok": check["ok"] == true,
        "at": now(), "secs": check["secs"], "model": check["model"],
        "error": check["error"].as_str().map(|e| e.chars().take(300).collect::<String>()),
        "how": how,
    }));
    // 每個 agent 只留最近 20 筆
    let mut keep = vec![];
    for r in all.iter().rev() {
        if keep
            .iter()
            .filter(|k: &&Value| k["agent"] == r["agent"])
            .count()
            < 20
        {
            keep.push(r.clone());
        }
    }
    keep.reverse();
    let f = file(instance);
    let _ = std::fs::create_dir_all(f.parent().unwrap());
    let tmp = f.with_extension(format!("json.tmp{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_string_pretty(&keep).unwrap()).is_ok() {
        let _ = std::fs::rename(&tmp, &f);
    }
}

/// 目前這一組（轉接器版本＋本體版本）的狀態：ok／fail／unknown，附最近一筆紀錄。
pub fn state(instance: &Path, agent: &str, adapter: Option<&str>, engine: Option<&str>) -> Value {
    let all = read(instance);
    let mine: Vec<&Value> = all.iter().filter(|r| r["agent"] == agent).collect();
    let hit = mine
        .iter()
        .rev()
        .find(|r| r["adapter"].as_str() == adapter && r["engine"].as_str() == engine);
    match hit {
        Some(r) => {
            let mut v = (*r).clone();
            v["state"] = json!(if r["ok"] == true { "ok" } else { "fail" });
            v
        }
        None => {
            let last = mine.last().map(|r| (*r).clone()).unwrap_or(Value::Null);
            json!({"state": "unknown", "last": last})
        }
    }
}
