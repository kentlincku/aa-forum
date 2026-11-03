//! 測試支援：假 ACP agent（stdio JSON-RPC），不連任何模型。
//! 行為：回覆 "ECHO: <prompt 最後一行>"；含 NEED_PERMISSION 先請求權限；含 SLOW 等到被取消；
//! HANG 卡住且不理會取消（模擬卡在斷線串流的 agent）；
//! EMPTY_REPLY 回 Hermes 的空回覆標記「(empty)」；
//! WHOAMI 回 "<session id> turn <n>"；MODEL? 回目前模型；支援 session/load。
//! 環境變數 FAKE_MODELS=config|models：session/new 以 configOptions 或 availableModels 回報模型 m1、fake/m2；
//! 切到 m-broken 後回覆供應商錯誤（實測模型失敗用）。
//! 權限模式：session/new 回報 modes（default／acceptEdits／bypassPermissions），支援 session/set_mode；MODE? 回目前模式。
//! FAKE_ALLOW_ALWAYS=1：權限請求多一個 allow_always 選項。

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::mpsc;

fn send(v: Value) {
    let mut o = std::io::stdout().lock();
    writeln!(o, "{v}").unwrap();
    o.flush().unwrap();
}

fn update(sid: &str, u: Value) {
    send(
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}}),
    );
}

/// 假 ACP agent 主程式：讀 stdin、寫 stdout。給測試用的 bin 呼叫。
/// 本體相容測試：FAKE_ENGINE_BIN 指到的本體 `--version` 含 "bad" 就拒絕開 session。
fn engine_bad() -> bool {
    let Ok(bin) = std::env::var("FAKE_ENGINE_BIN") else {
        return false;
    };
    std::process::Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("bad"))
        .unwrap_or(true)
}

pub fn fake_acp_main() {
    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    let mut sid = format!("sess-{}", std::process::id());
    let mut turns = 0u32;
    let mut failed_once = false;
    let mut model = "m1".to_string();
    let mut mode = "default".to_string();
    let mut effort = "medium".to_string();
    // 推理強度選項隨模型不同（m1：low／medium／high；其他：low／medium）
    let efforts = |model: &str| -> Vec<&'static str> {
        if model == "m1" {
            vec!["low", "medium", "high"]
        } else {
            vec!["low", "medium"]
        }
    };
    let config = |model: &str, effort: &str, opts: &[&str]| {
        json!([{"id": "model", "category": "model", "currentValue": model,
                "options": opts.iter().map(|o| json!({"value": o, "name": o})).collect::<Vec<_>>()},
               {"id": "effort", "category": "thought_level", "currentValue": effort,
                "options": efforts(model).iter().map(|o| json!({"value": o, "name": o})).collect::<Vec<_>>()}])
    };
    let always = std::env::var("FAKE_ALLOW_ALWAYS").is_ok();
    let report = std::env::var("FAKE_MODELS").unwrap_or_default();
    let mut next_id = 1000u64;
    while let Ok(msg) = rx.recv() {
        let id = msg["id"].clone();
        match msg["method"].as_str() {
            Some("initialize") => send(
                json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": true}, "agentInfo": {"name": "fake-acp", "version": "0"}, "authMethods": []}}),
            ),
            Some("session/new") if engine_bad() => send(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "engine version not supported"}}),
            ),
            Some("session/new") => {
                let mut r = json!({"sessionId": sid, "modes": {"currentModeId": mode, "availableModes": [
                    {"id": "default", "name": "Default"}, {"id": "acceptEdits", "name": "Accept edits"},
                    {"id": "bypassPermissions", "name": "Bypass permissions"}]}});
                let opts = ["m1", "fake/m2", "m-broken"];
                if report == "config" {
                    r["configOptions"] = config(&model, &effort, &opts);
                } else if report == "models" {
                    r["models"] = json!({"currentModelId": model,
                        "availableModels": opts.iter().map(|o| json!({"modelId": o, "name": o})).collect::<Vec<_>>()});
                }
                send(json!({"jsonrpc": "2.0", "id": id, "result": r}))
            }
            Some("session/set_model") if report == "models" => {
                model = msg["params"]["modelId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                send(json!({"jsonrpc": "2.0", "id": id, "result": {}}))
            }
            Some("session/set_config_option")
                if report == "config" && msg["params"]["configId"] == "effort" =>
            {
                let want = msg["params"]["value"].as_str().unwrap_or_default();
                if efforts(&model).contains(&want) {
                    effort = want.to_string();
                    send(
                        json!({"jsonrpc": "2.0", "id": id, "result": {"configOptions": config(&model, &effort, &["m1", "fake/m2", "m-broken"])}}),
                    )
                } else {
                    send(
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "unknown effort"}}),
                    )
                }
            }
            Some("session/set_config_option") if report == "config" => {
                model = msg["params"]["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                if !efforts(&model).contains(&effort.as_str()) {
                    effort = "medium".into();
                }
                send(
                    json!({"jsonrpc": "2.0", "id": id, "result": {"configOptions": config(&model, &effort, &["m1", "fake/m2", "m-broken"])}}),
                )
            }
            Some("session/set_mode") => {
                let want = msg["params"]["modeId"].as_str().unwrap_or_default();
                if ["default", "acceptEdits", "bypassPermissions"].contains(&want) {
                    mode = want.to_string();
                    send(json!({"jsonrpc": "2.0", "id": id, "result": {}}))
                } else {
                    send(
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "unknown mode"}}),
                    )
                }
            }
            Some("session/load") if std::env::var_os("FAKE_FAIL_LOAD").is_some() => send(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"session unavailable"}}),
            ),
            Some("session/load") => {
                // 假 agent 不保存歷史：沿用 id，回合數從 0 起算以外不變；重播一則歷史訊息
                sid = msg["params"]["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                update(
                    &sid,
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "REPLAYED HISTORY"}}),
                );
                send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
            }
            Some("session/prompt") => {
                let text = msg["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                // Exact, opt-in fault command for in-place dead-child recovery tests.
                if text == "DIE" {
                    std::process::exit(0);
                }
                turns += 1;
                let mut stop = "end_turn";
                if text.contains("TOOL_EVIDENCE") {
                    update(
                        &sid,
                        json!({"sessionUpdate":"tool_call_update","toolCallId":"evidence-1","title":"inspect artifact","status":"completed","rawInput":{"command":"DO-NOT-REPLAY"},"rawOutput":{"stdout":"RAW-CANARY M artifact.txt (uncommitted)","exit_code":0},"locations":[{"path":"artifact.txt"}]}),
                    );
                }
                if text.contains("USAGE") {
                    let mut u =
                        json!({"sessionUpdate":"usage_update","used": turns * 1000,"size": 200000});
                    if text.contains("RATELIMIT") {
                        u["_meta"] = json!({"_claude/rateLimit": {"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.1,"resetsAt":1793354400}}}});
                    }
                    update(&sid, u);
                }
                let mut reply = format!("ECHO: {}", text.lines().last().unwrap_or_default());
                if std::env::var_os("FAKE_ECHO_PROMPT").is_some() {
                    reply = text.clone();
                }
                if text.contains("WHOAMI") {
                    reply = format!("{sid} turn {turns}");
                }
                if text.contains("MODE?") {
                    reply = format!("MODE: {mode}");
                }
                if text.contains("MODEL?") {
                    reply = format!("MODEL: {model}");
                }
                if text.contains("ENV?") {
                    reply = format!(
                        "ENV: {}",
                        std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| "-".into())
                    );
                }
                if text.contains("EFFORT?") {
                    reply = format!("EFFORT: {effort}");
                }
                if text.contains("EMPTY_REPLY") {
                    reply = "(empty)".into();
                }
                if model == "m-broken" {
                    reply = "API Error: model not found".into();
                }
                if text.contains("ERRCOPY") {
                    reply = "GitHub Copilot rejected this request as malformed, so the model didn't answer. Start a clean session with /new.".into();
                }
                // Codex 連線失敗時回的警告（實際文字）；_ONCE 只在第一輪
                if text.contains("CODEX_WARN_ALWAYS")
                    || (text.contains("CODEX_WARN_ONCE") && turns == 1)
                {
                    reply = "Warning: Falling back from WebSockets to HTTPS transport. workspace routing discovery timed out\n\nworkspace routing discovery timed out".into();
                }
                if text.contains("STOP_MAXTOKENS") {
                    stop = "max_tokens";
                    reply = "PARTIAL answer".into();
                }
                if text.contains("STOP_REFUSAL") {
                    stop = "refusal";
                    reply = String::new();
                }
                if text.contains("ALWAYS_FAIL") || (text.contains("FAIL_ONCE") && !failed_once) {
                    failed_once = true;
                    send(
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "upstream HTTP 499"}}),
                    );
                    continue;
                }
                if text.contains("NEED_PERMISSION") {
                    update(
                        &sid,
                        json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "write file", "status": "pending"}),
                    );
                    next_id += 1;
                    send(
                        json!({"jsonrpc": "2.0", "id": next_id, "method": "session/request_permission", "params": {
                        "sessionId": sid.clone(), "toolCall": {"toolCallId": "t1", "title": "write file"},
                        "options": if always {
                            json!([{"optionId": "deny", "name": "Deny", "kind": "reject_once"}, {"optionId": "allow", "name": "Allow", "kind": "allow_once"}, {"optionId": "always", "name": "Always", "kind": "allow_always"}])
                        } else {
                            json!([{"optionId": "allow", "name": "Allow", "kind": "allow_once"}, {"optionId": "deny", "name": "Deny", "kind": "reject_once"}])
                        }}}),
                    );
                    loop {
                        let m = rx.recv().unwrap();
                        if m["id"] == json!(next_id) {
                            let o = &m["result"]["outcome"];
                            reply = match (o["outcome"].as_str(), o["optionId"].as_str()) {
                                (Some("selected"), Some(opt)) => format!("PERMISSION: {opt}"),
                                _ => {
                                    stop = "cancelled";
                                    "PERMISSION: cancelled".into()
                                }
                            };
                            break;
                        }
                        if m["method"] == "session/cancel" {
                            stop = "cancelled";
                        }
                    }
                } else if text.contains("HANG") {
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                } else if text.contains("STEERABLE") && !text.starts_with("/steer") {
                    // 模擬 Hermes：回合中收到 `/steer x` 先回條（agent 訊息＋end_turn），
                    // 收到含 FINISH 的插入後結束本輪，回覆列出收到的插入；可被取消
                    let mut got: Vec<String> = Vec::new();
                    loop {
                        let m = rx.recv().unwrap();
                        if m["method"] == "session/cancel" {
                            stop = "cancelled";
                            reply = String::new();
                            break;
                        }
                        if m["method"] == "session/prompt" {
                            let t = m["params"]["prompt"][0]["text"]
                                .as_str()
                                .unwrap_or_default();
                            if let Some(body) = t.strip_prefix("/steer") {
                                let body = body.trim().to_string();
                                let preview: String = if body.chars().count() > 80 {
                                    format!("{}...", body.chars().take(80).collect::<String>())
                                } else {
                                    body.clone()
                                };
                                update(
                                    &sid,
                                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("⏩ Steer queued for the active turn: {preview}")}}),
                                );
                                send(
                                    json!({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}}),
                                );
                                let fin = body.contains("FINISH");
                                got.push(body.lines().last().unwrap_or_default().to_string());
                                if fin {
                                    break;
                                }
                            }
                        }
                    }
                    if stop != "cancelled" {
                        reply = format!("STEERED: {}", got.join(" | "));
                    }
                } else if text.contains("SLOW") {
                    loop {
                        let m = rx.recv().unwrap();
                        if m["method"] == "session/cancel" {
                            stop = "cancelled";
                            reply = String::new();
                            break;
                        }
                    }
                }
                if !reply.is_empty() {
                    let mut mid = reply.len() / 2;
                    while !reply.is_char_boundary(mid) {
                        mid += 1;
                    }
                    for chunk in [&reply[..mid], &reply[mid..]] {
                        update(
                            &sid,
                            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": chunk}}),
                        );
                    }
                }
                send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": stop}}));
            }
            Some(m) if !id.is_null() => {
                send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": m}}))
            }
            _ => {}
        }
    }
}
