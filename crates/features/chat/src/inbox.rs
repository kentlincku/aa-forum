//! SQLite 信箱：dispatcher 的 Inbox 實作。lane＝角色＋群；不開每群 lane 的角色所有信走預設 lane。

use crate::store;
use aaf_core::events::EventBus;
use aaf_core::storage::Db;
use aaf_core::Result;
use aaf_runtime::dispatcher::{Inbox, InboxMessage, Lane, SharedRoster};
use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

pub struct ChatInbox {
    pub db: Db,
    pub bus: EventBus,
    pub roster: SharedRoster,
    pub hooks: Option<aaf_runtime::hooks::Hooks>,
}

/// 可喚醒條件：待投遞、群為 active 且未凍結。
const WAKEABLE: &str = "d.state = 'queued' AND r.state = 'active' AND r.frozen = 0";

#[async_trait]
impl Inbox for ChatInbox {
    async fn lanes_with_unread(&self) -> Result<Vec<Lane>> {
        self.db.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT DISTINCT d.recipient, m.room FROM chat_deliveries d JOIN chat_messages m ON m.id = d.message
                 JOIN chat_rooms r ON r.id = m.room WHERE {WAKEABLE}"
            ))?;
            let v = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .map(|x| x.map(|(role, room)| self.roster.read().unwrap().lane_for(&role, Some(room))))
                .collect::<rusqlite::Result<_>>()?;
            Ok(v)
        })
    }

    async fn unread(&self, lane: &Lane) -> Result<Vec<InboxMessage>> {
        self.db.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT m.id, m.author, m.body, m.room, m.thread_id FROM chat_deliveries d JOIN chat_messages m ON m.id = d.message
                 JOIN chat_rooms r ON r.id = m.room WHERE {WAKEABLE} AND d.recipient = ?1 AND (?2 IS NULL OR m.room = ?2) ORDER BY m.id"
            ))?;
            let v = st
                .query_map(params![lane.role, lane.room], |r| {
                    Ok(InboxMessage { id: r.get(0)?, from: r.get(1)?, body: r.get(2)?, room: Some(r.get(3)?), thread_id: r.get(4)? })
                })?
                .collect::<rusqlite::Result<Vec<InboxMessage>>>()?;
            // 附件：在內文後面列出名稱、大小與本機路徑（agent 以唯讀方式自行開檔）
            let ids: Vec<i64> = v.iter().map(|m| m.id).collect();
            let mut att = crate::attachments::for_messages(c, &ids)?;
            let inst = self.roster.read().unwrap().instance.clone();
            let v = v
                .into_iter()
                .map(|mut m| {
                    for a in att.remove(&m.id).unwrap_or_default() {
                        let p = crate::attachments::blob_path(&inst, &a.sha256).map(|p| p.display().to_string()).unwrap_or_default();
                        let icon = if a.kind == "image" { "🖼" } else { "📎" };
                        m.body.push_str(&format!("\n{icon} {} ({}, read-only copy at {p})", a.name, crate::attachments::human_size(a.size)));
                    }
                    m
                })
                .collect();
            Ok(v)
        })
    }

    async fn ack(&self, role: &str, ids: &[i64]) -> Result<()> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            for id in ids {
                tx.execute("UPDATE chat_deliveries SET state = 'done' WHERE message = ?1 AND recipient = ?2 AND state = 'queued'", params![id, role])?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    async fn reply(&self, role: &str, to: &InboxMessage, body: &str) -> Result<()> {
        let room = to
            .room
            .ok_or_else(|| aaf_core::Error::Other("reply without room".into()))?;
        let roster = self.roster.read().unwrap().clone();
        let rank = roster
            .roles
            .get(role)
            .map(|r| r.rank.clone())
            .unwrap_or_else(|| "worker".into());
        let ev = serde_json::json!({"type": "chat.post", "sender": role, "rank": rank, "room": room, "body": body, "reply_to": to.id});
        if let Some(h) = &self.hooks {
            if let Some(why) = h.pre(&ev).await {
                // 擋下：不發文；理由下次叫醒時告訴該角色
                h.add_notes(
                    role,
                    &[format!(
                        "Your reply in room {room} was NOT posted — blocked by rule: {why}"
                    )],
                );
                self.bus.publish(
                    "chat.message.blocked",
                    1,
                    serde_json::json!({"room": room, "sender": role, "reason": why}),
                )?;
                return Ok(());
            }
        }
        let (res, evs) = self.db.with(|c| {
            let tx = c.transaction()?;
            let mut evs = Vec::new();
            // 群已非 active（回合進行中被封存／刪除）時丟棄回覆，不報錯
            let res = store::post_message(
                &tx,
                &mut evs,
                &roster,
                room,
                role,
                "role",
                body,
                Some(to.id),
            );
            if res.is_ok() {
                tx.commit()?;
            }
            Ok((res.map(|_| ()), evs))
        })?;
        for e in evs {
            self.bus.announce(e);
        }
        match res {
            Err(e) => tracing::info!(%role, room, error = %e, "reply dropped"),
            Ok(()) => {
                if let Some(h) = &self.hooks {
                    h.post(&ev).await;
                }
            }
        }
        Ok(())
    }

    async fn notify(&self, room: Option<i64>, body: &str) -> Result<Option<i64>> {
        let Some(room) = room else {
            return Ok(None);
        };
        let roster = self.roster.read().unwrap().clone();
        let (res, evs) = self.db.with(|c| {
            let tx = c.transaction()?;
            let mut evs = Vec::new();
            let res = store::post_system(&tx, &mut evs, &roster, room, body);
            if res.is_ok() {
                tx.commit()?;
            }
            Ok((res.map(|m| m.id), evs))
        })?;
        for e in evs {
            self.bus.announce(e);
        }
        match res {
            Ok(id) => Ok(Some(id)),
            Err(e) => {
                tracing::info!(room, error = %e, "system notice dropped");
                Ok(None)
            }
        }
    }

    async fn load_session(&self, lane: &Lane) -> Result<Option<String>> {
        self.db.with(|c| {
            Ok(c.query_row(
                "SELECT session_id FROM chat_sessions WHERE role = ?1 AND room = ?2",
                params![lane.role, lane.room.unwrap_or(0)],
                |r| r.get(0),
            )
            .optional()?)
        })
    }

    async fn forget_sessions(&self, role: &str) -> Result<()> {
        self.db.with(|c| {
            Ok(
                c.execute("DELETE FROM chat_sessions WHERE role = ?1", [role])
                    .map(|_| ())?,
            )
        })
    }

    async fn save_session(&self, lane: &Lane, session_id: &str) -> Result<()> {
        self.db.with(|c| {
            if session_id.is_empty() {
                c.execute("DELETE FROM chat_sessions WHERE role = ?1 AND room = ?2", params![lane.role, lane.room.unwrap_or(0)])?;
            } else {
                c.execute(
                    "INSERT INTO chat_sessions(role, room, session_id) VALUES (?1, ?2, ?3) ON CONFLICT(role, room) DO UPDATE SET session_id = excluded.session_id",
                    params![lane.role, lane.room.unwrap_or(0), session_id],
                )?;
            }
            Ok(())
        })
    }
}
