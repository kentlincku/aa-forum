//! agent／人用的 API 用戶端子指令：`aaf task …`、`aaf chat …`。
//! 身分：AAF_TOKEN（角色 token 或人類 session token）；位址：AAF_URL（預設 http://127.0.0.1:8111）。
//! 只用標準函式庫的 TCP 送 HTTP/1.1，不另外引入 HTTP 套件。

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};
use std::io::{Read, Write};

#[derive(Subcommand)]
pub enum TaskCmd {
    /// 列出任務（預設：我所在群的未完成任務）
    List {
        #[arg(long)]
        state: Option<String>,
        /// 只列我發起或指派給我的
        #[arg(long)]
        mine: bool,
        /// 只列某群（預設 AAF_ROOM）
        #[arg(long)]
        room: Option<i64>,
    },
    /// 建立任務（預設在 AAF_ROOM）
    New {
        title: String,
        #[arg(long, default_value = "")]
        spec: String,
        /// 指派對象（必須是群成員）
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        room: Option<i64>,
    },
    /// 認領（或續租）任務
    Claim { id: i64 },
    /// 回報完成
    Done { id: i64, result: Option<String> },
    /// 回報受阻
    Blocked { id: i64, reason: Option<String> },
    /// 取消（發起人）
    Cancel { id: i64, reason: Option<String> },
}

#[derive(Subcommand)]
pub enum ChatCmd {
    /// 在群裡發言（預設 AAF_ROOM）；內容可用 - 從 stdin 讀；--file 可附檔（可重複）
    Post {
        #[arg(default_value = "")]
        text: String,
        #[arg(long = "file")]
        files: Vec<std::path::PathBuf>,
        #[arg(long)]
        room: Option<i64>,
        #[arg(long)]
        reply: Option<i64>,
    },
    /// 讀群裡最近的訊息
    Read {
        #[arg(long)]
        room: Option<i64>,
        #[arg(long, default_value_t = 20)]
        last: usize,
    },
    /// 我是誰（驗證 token）
    Whoami,
}

struct Client {
    host: String,
    token: String,
}

impl Client {
    fn from_env() -> Result<Self> {
        let url = std::env::var("AAF_URL").unwrap_or_else(|_| "http://127.0.0.1:8111".into());
        let host = url
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("AAF_URL must be http://host:port (local only)"))?
            .trim_end_matches('/')
            .to_string();
        let token = std::env::var("AAF_TOKEN").context("AAF_TOKEN is not set")?;
        Ok(Self { host, token })
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let b = body.map(|v| v.to_string()).unwrap_or_default();
        self.raw(method, path, "application/json", b.as_bytes())
    }

    fn upload(&self, path: &str, bytes: &[u8]) -> Result<Value> {
        self.raw("POST", path, "application/octet-stream", bytes)
    }

    fn raw(&self, method: &str, path: &str, ctype: &str, b: &[u8]) -> Result<Value> {
        let mut s = std::net::TcpStream::connect(&self.host)
            .with_context(|| format!("connect {}", self.host))?;
        let head = format!(
            "{method} /api/v1{path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAuthorization: Bearer {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n",
            self.host,
            self.token,
            b.len()
        );
        s.write_all(head.as_bytes())?;
        s.write_all(b)?;
        let mut out = Vec::new();
        s.read_to_end(&mut out)?;
        let text = String::from_utf8_lossy(&out);
        let (head, rest) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| anyhow!("bad response"))?;
        let status: u16 = head
            .split(' ')
            .nth(1)
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(rest)
        } else {
            rest.to_string()
        };
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            bail!("{status}: {}", v["message"].as_str().unwrap_or(&body));
        }
        Ok(v)
    }
}

fn dechunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some((len, tail)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(len.trim(), 16).unwrap_or(0);
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

fn room_or_env(r: Option<i64>) -> Result<i64> {
    match r {
        Some(r) => Ok(r),
        None => std::env::var("AAF_ROOM")
            .context("no --room and AAF_ROOM is not set")?
            .parse()
            .context("AAF_ROOM"),
    }
}

fn text_arg(t: &str) -> Result<String> {
    if t == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        Ok(t.to_string())
    }
}

fn print_tasks(v: &Value) {
    for t in v.as_array().into_iter().flatten() {
        println!(
            "#{} [{}] {}  (room {}, owner {}, assignee {})",
            t["id"],
            t["state"].as_str().unwrap_or(""),
            t["title"].as_str().unwrap_or(""),
            t["room"],
            t["owner"].as_str().unwrap_or(""),
            t["assignee"].as_str().unwrap_or("-")
        );
    }
}

pub fn task(cmd: TaskCmd) -> Result<()> {
    let c = Client::from_env()?;
    match cmd {
        TaskCmd::List { state, mine, room } => {
            let q = |s: &Option<String>| {
                s.as_ref()
                    .map(|x| format!("state={x}&"))
                    .unwrap_or_default()
            };
            let v = match room
                .or_else(|| std::env::var("AAF_ROOM").ok().and_then(|r| r.parse().ok()))
            {
                Some(r) if !mine => {
                    c.call("GET", &format!("/chat/rooms/{r}/tasks?{}", q(&state)), None)?
                }
                _ => c.call(
                    "GET",
                    &format!("/chat/tasks?{}mine={mine}", q(&state)),
                    None,
                )?,
            };
            print_tasks(&v);
        }
        TaskCmd::New {
            title,
            spec,
            to,
            room,
        } => {
            let r = room_or_env(room)?;
            let v = c.call(
                "POST",
                &format!("/chat/rooms/{r}/tasks"),
                Some(json!({"title": title, "spec": text_arg(&spec)?, "assignee": to})),
            )?;
            println!("task #{} created", v["id"]);
        }
        TaskCmd::Claim { id } => {
            let v = c.call("POST", &format!("/chat/tasks/{id}/claim"), None)?;
            println!("task #{id} claimed until {}", v["lease_until"]);
        }
        TaskCmd::Done { id, result } => report(&c, id, "done", result)?,
        TaskCmd::Blocked { id, reason } => report(&c, id, "blocked", reason)?,
        TaskCmd::Cancel { id, reason } => report(&c, id, "cancelled", reason)?,
    }
    Ok(())
}

fn report(c: &Client, id: i64, state: &str, text: Option<String>) -> Result<()> {
    let result = match text {
        Some(t) => text_arg(&t)?,
        None => String::new(),
    };
    c.call(
        "POST",
        &format!("/chat/tasks/{id}/report"),
        Some(json!({"state": state, "result": result})),
    )?;
    println!("task #{id} → {state}");
    Ok(())
}

pub fn chat(cmd: ChatCmd) -> Result<()> {
    let c = Client::from_env()?;
    match cmd {
        ChatCmd::Post {
            text,
            files,
            room,
            reply,
        } => {
            let r = room_or_env(room)?;
            let mut ids = Vec::new();
            for f in &files {
                let name = f
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| anyhow!("bad file name: {}", f.display()))?;
                let bytes = std::fs::read(f).with_context(|| format!("read {}", f.display()))?;
                let q: String = name
                    .bytes()
                    .map(|b| {
                        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                            (b as char).to_string()
                        } else {
                            format!("%{b:02X}")
                        }
                    })
                    .collect();
                let a = c.upload(&format!("/chat/rooms/{r}/attachments?name={q}"), &bytes)?;
                ids.push(a["id"].clone());
            }
            let body = text_arg(&text)?;
            if body.trim().is_empty() && ids.is_empty() {
                bail!("nothing to post");
            }
            let v = c.call(
                "POST",
                &format!("/chat/rooms/{r}/messages"),
                Some(json!({"body": body, "reply_to": reply, "attachments": ids})),
            )?;
            println!(
                "posted #{} (woke: {})",
                v["message"]["id"], v["delivered_to"]
            );
        }
        ChatCmd::Read { room, last } => {
            let r = room_or_env(room)?;
            let v = c.call("GET", &format!("/chat/rooms/{r}/messages?limit=500"), None)?;
            let all = v.as_array().cloned().unwrap_or_default();
            for m in all.iter().skip(all.len().saturating_sub(last)) {
                println!(
                    "#{} {}: {}",
                    m["id"],
                    m["author"].as_str().unwrap_or(""),
                    m["body"].as_str().unwrap_or("")
                );
            }
        }
        ChatCmd::Whoami => {
            // 有 AAF_ROOM 就問「在這個群」的 rank（群級 lead）
            let path = match room_or_env(None) {
                Ok(r) => format!("/chat/whoami?room={r}"),
                Err(_) => "/chat/whoami".to_string(),
            };
            let v = c.call("GET", &path, None)?;
            let name = v["name"].as_str().unwrap_or("");
            let rank = v["rank"].as_str().unwrap_or("");
            match (v["room"].as_i64(), v["global_rank"].as_str()) {
                (Some(r), Some(g)) if g != rank => {
                    println!("{name} ({rank} in room {r}; global {g})")
                }
                (Some(r), _) => println!("{name} ({rank} in room {r})"),
                _ => println!("{name} ({rank})"),
            }
        }
    }
    Ok(())
}
#[derive(Subcommand)]
pub enum ContextCmd {
    /// Read the source index or a full source page (JSON; follow next cursors)
    Read {
        #[arg(long)]
        room: Option<i64>,
        #[arg(long, env = "AAF_ROLE")]
        role: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        source: Option<i64>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Include immutable raw chunks instead of the logical source projection
        #[arg(long)]
        raw: bool,
    },
}

pub fn context(cmd: ContextCmd) -> Result<()> {
    let ContextCmd::Read {
        room,
        role,
        after,
        limit,
        source,
        offset,
        raw,
    } = cmd;
    let room = room_or_env(room)?;
    if room <= 0
        || !aaf_runtime::dispatcher::valid_role_name(&role)
        || after < 0
        || !(1..=100).contains(&limit)
        || source.is_some_and(|s| s <= 0)
        || (source.is_none() && offset != 0)
    {
        bail!("invalid context lane or cursor");
    }
    let mut path =
        format!("/context/rooms/{room}/roles/{role}/sources?after={after}&limit={limit}");
    if let Some(id) = source {
        path.push_str(&format!("&source={id}&offset={offset}"));
    }
    if raw {
        path.push_str("&raw=true");
    }
    let value = Client::from_env()?.call("GET", &path, None)?;
    if !value.is_object() || (!value["sources"].is_array() && !value["source"].is_i64()) {
        bail!("invalid context response JSON or schema");
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
