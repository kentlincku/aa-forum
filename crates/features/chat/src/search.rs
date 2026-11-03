//! 訊息搜尋（源自先前的 Python 版）。
//!
//! - FTS5 trigram：查詢 ≥3 字元走索引（每個詞都要出現，AND）；更短的詞改用 LIKE（中文兩字詞常見）。
//! - 範圍：只搜查詢者是成員的群（Owner 全部），且群為 active／archived（垃圾桶不搜）。
//! - 篩選：群、作者、提及我（@me）、有附件、日期（before／after，epoch 秒），依新到舊、可往前翻頁。

use rusqlite::params_from_iter;
use rusqlite::types::Value as Sql;
use serde::Serialize;

#[derive(Debug, Default)]
pub struct Query<'a> {
    pub text: &'a str,
    pub viewer: &'a str,
    pub all_rooms: bool,
    pub room: Option<i64>,
    pub from: Option<&'a str>,
    pub mentions_me: bool,
    pub has_attachment: bool,
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// 只回傳 id 小於此值的結果（往前翻頁）
    pub before_id: Option<i64>,
    pub limit: i64,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub id: i64,
    pub room: i64,
    pub room_name: String,
    pub author: String,
    pub author_kind: String,
    pub created_at: i64,
    pub snippet: String,
}

/// 把使用者輸入拆成詞：雙引號內視為一個片語，其餘以空白分隔。
pub fn terms(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in q.chars() {
        match c {
            '"' => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out.truncate(8);
    out
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 摘要：第一個命中詞前後各 40 字元。
fn snippet(body: &str, terms: &[String]) -> String {
    let lower = body.to_lowercase();
    let pos = terms
        .iter()
        .filter_map(|t| lower.find(&t.to_lowercase()))
        .min();
    let chars: Vec<char> = body.chars().collect();
    let (start, end) = match pos {
        Some(byte) => {
            let ci = body[..byte.min(body.len())].chars().count();
            (ci.saturating_sub(40), (ci + 80).min(chars.len()))
        }
        None => (0, chars.len().min(120)),
    };
    let mut s: String = chars[start..end]
        .iter()
        .collect::<String>()
        .replace('\n', " ");
    if start > 0 {
        s.insert(0, '…');
    }
    if end < chars.len() {
        s.push('…');
    }
    s
}

pub fn search(c: &rusqlite::Connection, q: &Query) -> aaf_core::Result<Vec<Hit>> {
    let ts = terms(q.text);
    let mut sql = String::from(
        "SELECT m.id, m.room, r.name, m.author, m.author_kind, m.created_at, m.body FROM chat_messages m JOIN chat_rooms r ON r.id = m.room WHERE r.state IN ('active','archived')",
    );
    let mut args: Vec<Sql> = Vec::new();
    let long: Vec<&String> = ts.iter().filter(|t| t.chars().count() >= 3).collect();
    if !long.is_empty() {
        // FTS5 片語：每個詞用雙引號包起來，內部雙引號跳脫；詞與詞 AND
        let expr = long
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        sql.push_str(
            " AND m.id IN (SELECT rowid FROM chat_messages_fts WHERE chat_messages_fts MATCH ?)",
        );
        args.push(Sql::Text(expr));
    }
    for t in ts.iter().filter(|t| t.chars().count() < 3) {
        sql.push_str(" AND m.body LIKE ? ESCAPE '\\'");
        args.push(Sql::Text(format!("%{}%", like_escape(t))));
    }
    if !q.all_rooms {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM chat_members x WHERE x.room = m.room AND x.member = ?)",
        );
        args.push(Sql::Text(q.viewer.to_string()));
    }
    if let Some(r) = q.room {
        sql.push_str(" AND m.room = ?");
        args.push(Sql::Integer(r));
    }
    if let Some(f) = q.from {
        sql.push_str(" AND m.author = ?");
        args.push(Sql::Text(f.to_string()));
    }
    if q.mentions_me {
        sql.push_str(" AND m.body LIKE ? ESCAPE '\\'");
        args.push(Sql::Text(format!("%@{}%", like_escape(q.viewer))));
    }
    if q.has_attachment {
        sql.push_str(" AND EXISTS (SELECT 1 FROM chat_attachments a WHERE a.message = m.id)");
    }
    if let Some(t) = q.since {
        sql.push_str(" AND m.created_at >= ?");
        args.push(Sql::Integer(t));
    }
    if let Some(t) = q.until {
        sql.push_str(" AND m.created_at < ?");
        args.push(Sql::Integer(t));
    }
    if let Some(b) = q.before_id {
        sql.push_str(" AND m.id < ?");
        args.push(Sql::Integer(b));
    }
    sql.push_str(" ORDER BY m.id DESC LIMIT ?");
    args.push(Sql::Integer(q.limit.clamp(1, 100)));
    let mut st = c.prepare(&sql)?;
    let rows = st.query_map(params_from_iter(args), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, room, room_name, author, author_kind, created_at, body) = row?;
        out.push(Hit {
            id,
            room,
            room_name,
            author,
            author_kind,
            created_at,
            snippet: snippet(&body, &ts),
        });
    }
    Ok(out)
}
