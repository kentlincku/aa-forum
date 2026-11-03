//! 群聊逐字稿匯出（源自先前的 Python 版）。
//!
//! - 每群一個資料夾：`<輸出根>/<群號>/<YYYY-MM-DD>.md`，另有 `_index.md`（每天則數與編號範圍）。
//! - 內容：留言原文（含角色、系統），回覆標 `↩ #id`，附件只列檔名與大小；未讀、已讀、投遞狀態不放。
//! - 冪等：整份重寫，但內容沒變就不寫（mtime 不動）；寫入走暫存檔＋rename。
//! - 範圍：使用中與封存的群（含凍結）；垃圾桶與已永久刪除的群不匯出。
//! - 時區：AAF_TZ（IANA 名稱），預設系統時區。
//! - 排程：AAF_TRANSCRIPT_MINUTES（預設 120，0＝停用）；也可 `aaf transcript` 或 API 立即匯出。
//! - 永久刪除群時：預設保留已匯出的逐字稿，可選擇一併刪除。

use aaf_core::{Core, Error, Result};
use rusqlite::params;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Report {
    pub rooms: usize,
    pub written: Vec<String>,
}

pub struct Options<'a> {
    pub root: &'a Path,
    pub tz: Option<&'a str>,
    pub rooms: Option<&'a [i64]>,
    /// YYYY-MM-DD：只重寫這天（含）以後；此時不重寫 _index.md
    pub since: Option<&'a str>,
}

fn tz(name: Option<&str>) -> jiff::tz::TimeZone {
    match name.filter(|n| !n.trim().is_empty()) {
        Some(n) => jiff::tz::TimeZone::get(n).unwrap_or_else(|_| {
            tracing::warn!(tz = n, "unknown AAF_TZ; using system time zone");
            jiff::tz::TimeZone::system()
        }),
        None => jiff::tz::TimeZone::system(),
    }
}

fn tz_label(z: &jiff::tz::TimeZone) -> String {
    let name = z.iana_name().unwrap_or("system").to_string();
    let off = z.to_offset(jiff::Timestamp::now());
    let secs = off.seconds();
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    format!("{name}（UTC{sign}{:02}:{:02}）", a / 3600, (a % 3600) / 60)
}

/// 內容相同就不寫；不同才以暫存檔 + rename 原子取代。回傳是否寫入。
fn write_if_changed(path: &Path, text: &str) -> Result<bool> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(false);
    }
    let dir = path
        .parent()
        .ok_or_else(|| Error::Other("bad transcript path".into()))?;
    std::fs::create_dir_all(dir).map_err(|e| Error::Other(format!("{}: {e}", dir.display())))?;
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text).map_err(|e| Error::Other(format!("{}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::Other(format!("{}: {e}", path.display())))?;
    Ok(true)
}

/// (id, name, state, frozen, members, messages)
type RoomDump = (i64, String, String, bool, Vec<String>, Vec<Msg>);

struct Msg {
    attachments: Vec<crate::attachments::Attachment>,
    id: i64,
    author: String,
    kind: String,
    body: String,
    reply_to: Option<i64>,
    created_at: i64,
}

pub fn export(core: &Core, o: &Options) -> Result<Report> {
    let z = tz(o.tz);
    let zone = tz_label(&z);
    // 一次讀出需要的資料（短交易），之後寫檔不佔 DB 連線
    let rooms: Vec<RoomDump> = core.db.with(|c| {
        let mut st = c.prepare("SELECT id, name, state, frozen FROM chat_rooms WHERE state IN ('active','archived') ORDER BY id")?;
        let all: Vec<(i64, String, String, bool)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for (id, name, state, frozen) in all {
            if o.rooms.is_some_and(|rs| !rs.contains(&id)) {
                continue;
            }
            let mut st = c.prepare("SELECT member FROM chat_members WHERE room = ?1 ORDER BY member")?;
            let members = st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
            let mut st = c.prepare("SELECT id, author, author_kind, body, reply_to, created_at FROM chat_messages WHERE room = ?1 ORDER BY id")?;
            let msgs = st
                .query_map(params![id], |r| {
                    Ok(Msg { attachments: vec![], id: r.get(0)?, author: r.get(1)?, kind: r.get(2)?, body: r.get(3)?, reply_to: r.get(4)?, created_at: r.get(5)? })
                })?
                .collect::<rusqlite::Result<Vec<Msg>>>()?;
            let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
            let mut att = crate::attachments::for_messages(c, &ids)?;
            let msgs = msgs
                .into_iter()
                .map(|mut m| {
                    m.attachments = att.remove(&m.id).unwrap_or_default();
                    m
                })
                .collect();
            out.push((id, name, state, frozen, members, msgs));
        }
        Ok(out)
    })?;

    let mut rep = Report {
        rooms: rooms.len(),
        written: vec![],
    };
    for (rid, name, state, frozen, members, msgs) in rooms {
        let mut days: BTreeMap<String, Vec<&Msg>> = BTreeMap::new();
        for m in &msgs {
            let ts = jiff::Timestamp::from_second(m.created_at)
                .map_err(|e| Error::Other(e.to_string()))?;
            let day = ts.to_zoned(z.clone()).strftime("%Y-%m-%d").to_string();
            if o.since.is_some_and(|s| day.as_str() < s) {
                continue;
            }
            days.entry(day).or_default().push(m);
        }
        let dir = o.root.join(rid.to_string());
        let status = match (state.as_str(), frozen) {
            ("archived", _) => "已封存",
            (_, true) => "已凍結",
            _ => "使用中",
        };
        let mut index = Vec::new();
        for (day, items) in &days {
            let (first, last) = (items[0].id, items[items.len() - 1].id);
            let mut lines = vec![
                format!("# 群 {rid}「{name}」逐字稿 · {day}"),
                String::new(),
                format!("- 成員：{}", members.join("、")),
                format!("- 時區：{zone}"),
                format!("- 本檔：{} 則，#{first}–#{last}", items.len()),
                format!("- 狀態：{status}"),
                format!("- 引用寫「群 {rid} #編號」；內文為原文，未改寫。"),
                String::new(),
            ];
            for m in items {
                let t = jiff::Timestamp::from_second(m.created_at)
                    .map_err(|e| Error::Other(e.to_string()))?;
                let who = match m.kind.as_str() {
                    "system" => "系統公告".to_string(),
                    "role" => format!("{}（agent）", m.author),
                    _ => m.author.clone(),
                };
                let mut head = format!(
                    "### #{} · {} · {who}",
                    m.id,
                    t.to_zoned(z.clone()).strftime("%H:%M:%S")
                );
                if let Some(r) = m.reply_to {
                    head.push_str(&format!(" ↩ #{r}"));
                }
                lines.push(head);
                if !m.body.is_empty() {
                    lines.push(m.body.clone());
                }
                // 附件只列檔名與大小，不複製檔案（同 Python 版）
                for a in &m.attachments {
                    let icon = if a.kind == "image" { "🖼" } else { "📎" };
                    lines.push(format!(
                        "{icon} {}（{}）",
                        a.name,
                        crate::attachments::human_size(a.size)
                    ));
                }
                lines.push(String::new());
            }
            let p = dir.join(format!("{day}.md"));
            if write_if_changed(&p, &lines.join("\n"))? {
                rep.written.push(p.display().to_string());
            }
            index.push(format!("| {day} | {} | #{first}–#{last} |", items.len()));
        }
        if !days.is_empty() && o.since.is_none() {
            let mut idx = vec![
                format!("# 群 {rid}「{name}」逐字稿索引"),
                String::new(),
                "| 日期 | 則數 | 編號範圍 |".into(),
                "|---|---|---|".into(),
            ];
            idx.extend(index);
            idx.push(String::new());
            let p = dir.join("_index.md");
            if write_if_changed(&p, &idx.join("\n"))? {
                rep.written.push(p.display().to_string());
            }
        }
    }
    Ok(rep)
}

/// 永久刪除群時一併刪除其逐字稿資料夾（只刪 `<root>/<id>`，id 為整數，不會跳出 root）。
pub fn remove_room(root: &Path, room: i64) -> Result<bool> {
    let d = root.join(room.to_string());
    if !d.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&d).map_err(|e| Error::Other(format!("{}: {e}", d.display())))?;
    Ok(true)
}

/// 輸出根目錄：AAF_TRANSCRIPT_DIR，空白＝`<instance>/outputs/transcripts`。
pub fn root(core: &Core, instance: &Path) -> PathBuf {
    match core.config.get("AAF_TRANSCRIPT_DIR").map(str::trim) {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => instance.join("outputs").join("transcripts"),
    }
}
