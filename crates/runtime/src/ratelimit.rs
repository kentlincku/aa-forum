//! Claude 額度視窗（5 小時／每週）：整理 Claude Code 每個回合附帶的 rate limit 資訊
//! （headless 是 stream-json 的 `rate_limit_event`；ACP 轉接器放在 usage_update 的 `_meta["_claude/rateLimit"]`）。
//! 不讀憑證，也不多花一個 token：資訊本來就跟著回合回來。token 登入的帳號也有。

use serde_json::{json, Value};

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// stream-json 的一行如果是 rate_limit_event，回傳它的 `rate_limit_info`。
pub fn from_stream_line(d: &Value) -> Option<&Value> {
    (d["type"] == "rate_limit_event").then(|| &d["rate_limit_info"])
}

fn label(key: &str) -> String {
    match key {
        "five_hour" => "5 hours".into(),
        "seven_day" => "Weekly".into(),
        k => match k.strip_prefix("seven_day_") {
            Some(m) => format!("Weekly · {m}"),
            None => k.into(),
        },
    }
}

/// 整理成額度頁用的格式：`{agent, account, observed_at, windows:[{label, remaining_percent, resets_at}]}`。
pub fn normalize(info: &Value, account: &str, observed_at: f64) -> Option<Value> {
    let windows: Vec<Value> = info["unifiedWindows"]
        .as_object()?
        .iter()
        .filter_map(|(k, w)| {
            let used = w["utilization"].as_f64().filter(|u| u.is_finite())?;
            let resets = w["resetsAt"].as_f64()?;
            Some(json!({
                "label": label(k),
                "remaining_percent": ((1.0 - used.clamp(0.0, 1.0)) * 100.0).round(),
                "resets_at": resets,
            }))
        })
        .collect();
    (!windows.is_empty()).then(|| {
        json!({"agent": "claude", "account": account, "observed_at": observed_at, "windows": windows})
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_unified_windows() {
        let info = json!({"status":"allowed","unifiedWindows":{
            "five_hour":{"utilization":0.03,"resetsAt":1793354400},
            "seven_day":{"utilization":0.15,"resetsAt":1793577600},
            "seven_day_opus":{"utilization":1.4,"resetsAt":1793577600},
            "bad":{"utilization":"x","resetsAt":1}}});
        let v = normalize(&info, "work", 5.0).unwrap();
        assert_eq!(v["account"], "work");
        let w: Vec<(String, f64)> = v["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| {
                (
                    x["label"].as_str().unwrap().into(),
                    x["remaining_percent"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            w,
            vec![
                ("5 hours".into(), 97.0),
                ("Weekly".into(), 85.0),
                ("Weekly · opus".into(), 0.0)
            ]
        );
        assert!(normalize(&json!({"status":"allowed"}), "d", 0.0).is_none());
    }

    #[test]
    fn picks_the_event_line() {
        let l = json!({"type":"rate_limit_event","rate_limit_info":{"a":1}});
        assert_eq!(from_stream_line(&l), Some(&json!({"a":1})));
        assert!(from_stream_line(&json!({"type":"result"})).is_none());
    }
}
