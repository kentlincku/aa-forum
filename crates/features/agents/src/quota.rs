//! 帳號額度（源自先前的 Python 版）。只讀本人 HOME，逾時短，不回傳憑證或帳號 ID。

use serde_json::{json, Value};
use std::time::Duration;

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

async fn run(exe: &str, args: &[&str], secs: u64) -> Result<Vec<u8>, String> {
    run_env(exe, args, &[], secs).await
}

/// 帶環境變數執行（多帳號：指到該帳號的設定資料夾）。
pub async fn run_env(
    exe: &str,
    args: &[&str],
    env: &[(String, String)],
    secs: u64,
) -> Result<Vec<u8>, String> {
    let path = aaf_runtime::catalog::which(exe).ok_or_else(|| format!("{exe} not found"))?;
    let o = tokio::time::timeout(
        Duration::from_secs(secs),
        tokio::process::Command::new(path)
            .args(args)
            .envs(env.iter().cloned())
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| format!("{exe} timed out"))?
    .map_err(|e| format!("{exe}: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "{exe} exited with {}",
            o.status.code().unwrap_or(-1)
        ));
    }
    Ok(o.stdout)
}

fn iso(s: &str) -> Option<f64> {
    s.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond() as f64 / 1000.0)
}

fn window(label: &str, used: &Value, resets: f64, now: f64) -> Option<Value> {
    let u = used
        .as_f64()
        .filter(|u| u.is_finite() && (0.0..=100.0).contains(u))?;
    (resets > now).then(
        || json!({"label": label, "remaining_percent": (100.0 - u).round(), "resets_at": resets}),
    )
}

/// GitHub Copilot（Hermes 預設 provider）：gh api /copilot_internal/user。
pub async fn copilot() -> Value {
    let mut out = json!({"engine": "Hermes · GitHub Copilot", "source": "gh api copilot_internal/user", "windows": []});
    let d: Value = match run("gh", &["api", "/copilot_internal/user"], 15)
        .await
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
    {
        Ok(d) => d,
        Err(e) => {
            out["error"] = json!(e);
            return out;
        }
    };
    let reset = d["quota_reset_date"]
        .as_str()
        .and_then(|s| iso(&format!("{s}T00:00:00Z")));
    let mut wins = vec![];
    for (key, snap) in d["quota_snapshots"].as_object().into_iter().flatten() {
        let name = match key.as_str() {
            "premium_interactions" => "Premium requests",
            "chat" => "Chat",
            "completions" => "Completions",
            k => k,
        };
        if snap["unlimited"] == true {
            wins.push(json!({"label": name, "remaining_percent": 100, "resets_at": reset, "unlimited": true}));
        } else if let (Some(ent), Some(pct)) = (
            snap["entitlement"].as_f64().filter(|e| *e > 0.0),
            snap["percent_remaining"].as_f64(),
        ) {
            wins.push(json!({"label": name, "count": format!("{}/{}", snap["remaining"].as_f64().unwrap_or(0.0) as i64, ent as i64), "remaining_percent": pct.round(), "resets_at": reset}));
        }
    }
    out["windows"] = json!(wins);
    out["plan"] = d["copilot_plan"].clone();
    out["observed_at"] = json!(now());
    out
}

/// Claude：`claude -p "/usage"`（Claude Code 自己的斜線指令，不開回合、不耗額度、不碰憑證）。
pub async fn claude() -> Value {
    claude_with(&[]).await
}

/// 指定帳號（CLAUDE_CONFIG_DIR 等環境變數）的 Claude 額度。
/// 用 OAuth token（`CLAUDE_CODE_OAUTH_TOKEN`）登入的帳號，`/usage` 不提供視窗用量，只回費用摘要：照實說。
pub async fn claude_with(env: &[(String, String)]) -> Value {
    let mut out = json!({"engine": "Claude", "source": "claude -p /usage", "windows": []});
    let text = match run_env(
        "claude",
        &["-p", "/usage", "--no-session-persistence"],
        env,
        20,
    )
    .await
    {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            out["error"] = json!(e);
            return out;
        }
    };
    let t = now();
    let wins = claude_windows(&text, t);
    if wins.is_empty() {
        out["note"] = json!(
            "this login does not report usage windows through /usage (token logins); press Check now to read them from a turn"
        );
    }
    out["windows"] = json!(wins);
    out["observed_at"] = json!(t);
    out
}

/// 送一個一個字的最便宜回合（haiku），從回合附帶的 `rate_limit_event` 取額度資訊。
/// token 登入的帳號 `/usage` 沒有視窗用量，要靠這個；會佔一點點額度，所以只在人按「重新整理」時跑。
pub async fn claude_probe(env: &[(String, String)]) -> Result<Value, String> {
    let out = run_env(
        "claude",
        &[
            "-p",
            "Reply with only: ok",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--model",
            "haiku",
        ],
        env,
        60,
    )
    .await?;
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|d| aaf_runtime::ratelimit::from_stream_line(&d).cloned())
        .ok_or_else(|| "the turn did not report rate limits".to_string())
}

/// 解析 `/usage` 的輸出：「Current session: 3% used · resets Oct 30 at 6pm (Asia/Taipei)」。
fn claude_windows(text: &str, now: f64) -> Vec<Value> {
    let mut wins = vec![];
    for line in text.lines() {
        let Some((label, rest)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim();
        if !label.starts_with("Current ") {
            continue;
        }
        let Some((used, _)) = rest.split_once('%') else {
            continue;
        };
        let Ok(used) = used.trim().parse::<f64>() else {
            continue;
        };
        let Some(resets) = rest
            .split_once("resets ")
            .and_then(|(_, r)| parse_reset(r.trim(), now))
        else {
            continue;
        };
        let label = match label {
            "Current session" => "5 hours".to_string(),
            "Current week (all models)" | "Current week" => "Weekly".to_string(),
            l => match l
                .strip_prefix("Current week (")
                .and_then(|x| x.strip_suffix(')'))
            {
                Some(m) => format!("Weekly · {m}"),
                None => l.to_string(),
            },
        };
        if let Some(w) = window(&label, &json!(used), resets, now) {
            wins.push(w);
        }
    }
    wins
}

/// 「Oct 30 at 6pm (Asia/Taipei)」「6:30pm (UTC)」→ epoch 秒；沒有日期就取下一次出現的那個時刻。
fn parse_reset(s: &str, now: f64) -> Option<f64> {
    let (body, tz) = match s.rfind('(') {
        Some(i) => (s[..i].trim(), s[i + 1..].trim_end_matches(')').trim()),
        None => (s.trim(), ""),
    };
    let zone = if tz.is_empty() {
        jiff::tz::TimeZone::system()
    } else {
        jiff::tz::TimeZone::get(tz).ok()?
    };
    let (date_part, time_part) = match body.split_once(" at ") {
        Some((d, t)) => (Some(d.trim()), t.trim()),
        None => (None, body),
    };
    let tl = time_part.to_lowercase();
    let (num, pm) = match (tl.strip_suffix("pm"), tl.strip_suffix("am")) {
        (Some(n), _) => (n, true),
        (_, Some(n)) => (n, false),
        _ => return None,
    };
    let (h, m) = match num.trim().split_once(':') {
        Some((h, m)) => (h.parse::<i8>().ok()?, m.parse::<i8>().ok()?),
        None => (num.trim().parse::<i8>().ok()?, 0),
    };
    let h24 = h % 12 + if pm { 12 } else { 0 };
    let nowz = jiff::Timestamp::from_second(now as i64)
        .ok()?
        .to_zoned(zone.clone());
    let at = |d: jiff::civil::Date| -> Option<jiff::Zoned> {
        d.at(h24, m, 0, 0).to_zoned(zone.clone()).ok()
    };
    let secs = |z: &jiff::Zoned| z.timestamp().as_second() as f64;
    match date_part {
        Some(d) => {
            let (mon, day) = d.split_once(' ')?;
            let month = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ]
            .iter()
            .position(|x| mon.to_lowercase().starts_with(x))? as i8
                + 1;
            let day: i8 = day.trim().parse().ok()?;
            let this = at(jiff::civil::Date::new(nowz.year(), month, day).ok()?)?;
            if secs(&this) >= now - 86400.0 {
                Some(secs(&this))
            } else {
                at(jiff::civil::Date::new(nowz.year() + 1, month, day).ok()?).map(|z| secs(&z))
            }
        }
        None => {
            let today = at(nowz.date())?;
            if secs(&today) > now {
                Some(secs(&today))
            } else {
                at(nowz.date().tomorrow().ok()?).map(|z| secs(&z))
            }
        }
    }
}

/// Codex：`codex app-server` 的 account/rateLimits/read（8 秒內）。
pub async fn codex() -> Value {
    codex_with(&[]).await
}

/// 指定帳號（CODEX_HOME 等環境變數）的 Codex 額度。
pub async fn codex_with(env: &[(String, String)]) -> Value {
    let mut out = json!({"engine": "Codex", "source": "codex app-server account/rateLimits/read", "windows": []});
    match codex_read(env).await {
        Err(e) => out["error"] = json!(e),
        Ok(r) => {
            let t = now();
            let mut wins = vec![];
            for (key, label) in [("primary", "5 hours"), ("secondary", "Weekly")] {
                let w = &r["rateLimits"][key];
                if let Some(resets) = w["resetsAt"].as_f64() {
                    if let Some(x) = window(label, &w["usedPercent"], resets, t) {
                        wins.push(x);
                    }
                }
            }
            out["windows"] = json!(wins);
            out["plan"] = r["rateLimits"]["planType"].clone();
            out["observed_at"] = json!(t);
        }
    }
    out
}

/// Antigravity（agy）：`agy -p /quota --output-format json`。唯讀斜線指令，不開回合、不耗額度。
pub async fn agy() -> Value {
    let mut out = json!({"engine": "Antigravity", "source": "agy -p /quota", "windows": []});
    match run("agy", &["-p", "/quota", "--output-format", "json"], 20)
        .await
        .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()))
    {
        Err(e) => out["error"] = json!(e),
        Ok(d) => {
            let t = now();
            out["windows"] = json!(agy_windows(&d["command"]["data"], t));
            out["observed_at"] = json!(t);
        }
    }
    out
}

/// 每個 group 的每個 bucket 一列，例如「Gemini · 5 hours」「Gemini 4 Argon · Weekly」。
fn agy_windows(data: &Value, now: f64) -> Vec<Value> {
    let mut wins = vec![];
    for g in data["groups"].as_array().into_iter().flatten() {
        let group = match g["name"].as_str().unwrap_or("") {
            "Gemini Models" => "Gemini",
            "Claude and GPT models" => "Claude/GPT",
            n => n,
        };
        for b in g["buckets"].as_array().into_iter().flatten() {
            let span = match b["window"].as_str().unwrap_or("") {
                "5h" => "5 hours",
                "weekly" => "Weekly",
                w => w,
            };
            let (Some(left), Some(resets)) = (
                b["remaining_fraction"].as_f64(),
                b["reset_time"].as_str().and_then(iso),
            ) else {
                continue;
            };
            if let Some(w) = window(
                &format!("{group} · {span}"),
                &json!(100.0 - left * 100.0),
                resets,
                now,
            ) {
                wins.push(w);
            }
        }
    }
    wins
}

async fn codex_read(env: &[(String, String)]) -> Result<Value, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let exe = aaf_runtime::catalog::which("codex").ok_or("codex not found")?;
    let mut ch = tokio::process::Command::new(exe)
        .arg("app-server")
        .envs(env.iter().cloned())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = ch.stdin.take().unwrap();
    let mut lines = tokio::io::BufReader::new(ch.stdout.take().unwrap()).lines();
    let work = async {
        stdin.write_all(b"{\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"aaf_quota\",\"version\":\"1\"}}}\n").await.map_err(|e| e.to_string())?;
        while let Some(l) = lines.next_line().await.map_err(|e| e.to_string())? {
            if l.len() > 1 << 20 {
                return Err("oversized response".into());
            }
            let Ok(d) = serde_json::from_str::<Value>(&l) else {
                continue;
            };
            if d.get("error").is_some() && d["id"].is_number() {
                return Err("quota unavailable".into());
            }
            if d["id"] == 1 {
                stdin.write_all(b"{\"method\":\"initialized\"}\n{\"id\":2,\"method\":\"account/rateLimits/read\"}\n").await.map_err(|e| e.to_string())?;
            }
            if d["id"] == 2 {
                return Ok(d["result"].clone());
            }
        }
        Err("codex app-server exited".into())
    };
    let r = tokio::time::timeout(Duration::from_secs(8), work)
        .await
        .map_err(|_| "quota timeout".to_string())?;
    let _ = ch.kill().await;
    r
}

/// 已安裝的 agent 各回一筆；沒有額度概念或查不到的回原因。
pub async fn all(installed: &[(String, String)], in_use: &[String]) -> Vec<Value> {
    let mut out = vec![];
    for (id, label) in installed {
        let mut item = match id.as_str() {
            "hermes" => copilot().await,
            "claude" => claude().await,
            "codex" => codex().await,
            "agy" => agy().await,
            "pi" => {
                json!({"engine": label, "note": "uses a local model; no account quota", "windows": []})
            }
            _ => {
                json!({"engine": label, "note": "this agent does not report quota", "windows": []})
            }
        };
        item["agent"] = json!(id);
        item["in_use"] = json!(in_use.contains(id));
        if item["windows"].as_array().is_none_or(|w| w.is_empty())
            && item.get("note").is_none()
            && item.get("error").is_none()
        {
            item["note"] = json!("nothing to show right now");
        }
        out.push(item);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_parse() {
        assert_eq!(super::iso("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(
            super::iso("2026-10-12T08:00:00+08:00"),
            super::iso("2026-10-12T00:00:00Z")
        );
        assert_eq!(
            super::iso("2026-10-12T00:00:00.5Z").unwrap()
                - super::iso("2026-10-12T00:00:00Z").unwrap(),
            0.5
        );
        assert_eq!(super::iso("2000-03-01T00:00:00Z"), Some(951868800.0));
        assert_eq!(super::iso("garbage"), None);
    }

    #[test]
    fn agy_windows_from_quota_json() {
        let data = serde_json::json!({"groups": [
            {"name": "Gemini Models", "buckets": [
                {"window": "weekly", "remaining_fraction": 0.9818, "reset_time": "2026-10-24T13:52:55Z"},
                {"window": "5h", "remaining_fraction": 1, "reset_time": "2026-10-22T14:23:21Z"}]},
            {"name": "Gemini 4 Argon", "buckets": [
                {"window": "weekly", "remaining_fraction": 0.9187, "reset_time": "2026-10-24T13:54:25Z"},
                {"window": "weekly", "remaining_fraction": 0.5, "reset_time": "2020-01-01T00:00:00Z"},
                {"window": "weekly", "reset_time": "2026-10-24T13:54:25Z"}]}]});
        let now = super::iso("2026-10-22T10:00:00Z").unwrap();
        let w = super::agy_windows(&data, now);
        let got: Vec<(&str, f64)> = w
            .iter()
            .map(|x| {
                (
                    x["label"].as_str().unwrap(),
                    x["remaining_percent"].as_f64().unwrap(),
                )
            })
            .collect();
        // 已過期的視窗、缺欄位的 bucket 略過
        assert_eq!(
            got,
            vec![
                ("Gemini · Weekly", 98.0),
                ("Gemini · 5 hours", 100.0),
                ("Gemini 4 Argon · Weekly", 92.0)
            ]
        );
    }

    #[test]
    fn claude_usage_output() {
        let now = super::iso("2026-10-30T04:00:00Z").unwrap();
        let text = "You are currently using your subscription\n\nCurrent session: 3% used · resets Oct 30 at 6pm (Asia/Taipei)\nCurrent week (all models): 15% used · resets Nov 2 at 8am (Asia/Taipei)\nCurrent week (Fable): 0% used · resets Nov 2 at 8am (Asia/Taipei)\n\nLast 24h · 1802 requests\n";
        let w = super::claude_windows(text, now);
        let got: Vec<(&str, f64, f64)> = w
            .iter()
            .map(|x| {
                (
                    x["label"].as_str().unwrap(),
                    x["remaining_percent"].as_f64().unwrap(),
                    x["resets_at"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("5 hours", 97.0, super::iso("2026-10-30T10:00:00Z").unwrap()),
                ("Weekly", 85.0, super::iso("2026-11-02T00:00:00Z").unwrap()),
                (
                    "Weekly · Fable",
                    100.0,
                    super::iso("2026-11-02T00:00:00Z").unwrap()
                ),
            ]
        );
        // token 登入只回費用摘要：沒有視窗
        assert!(
            super::claude_windows("Total cost:  $0.0000\nTotal duration (API): 0s\n", now)
                .is_empty()
        );
        assert_eq!(
            super::parse_reset("6:30pm (UTC)", now),
            super::iso("2026-10-30T18:30:00Z")
        );
        assert_eq!(
            super::parse_reset("3am (UTC)", now),
            super::iso("2026-10-31T03:00:00Z")
        );
        assert_eq!(
            super::parse_reset("Jan 2 at 9am (UTC)", now),
            super::iso("2027-01-02T09:00:00Z")
        );
        assert_eq!(super::parse_reset("garbage", now), None);
    }
}
