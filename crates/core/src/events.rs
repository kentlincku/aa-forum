//! 事件匯流排：先寫入 events 表（真相來源），commit 後再廣播。
//! 訂閱者落後（lagged）時，自動從資料庫依序號補讀，不會漏事件。

use crate::storage::{now, Db};
use crate::{Error, Result};
use aaf_contract::EventEnvelope;
use rusqlite::Transaction;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct EventBus {
    db: Db,
    tx: broadcast::Sender<EventEnvelope>,
}

/// 事件 kind 格式：`<namespace>.<name>[.<name>…]`，小寫英數與底線。
pub fn validate_kind(kind: &str) -> Result<()> {
    let ok = kind.split('.').count() >= 2
        && kind.split('.').all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        });
    if ok {
        Ok(())
    } else {
        Err(Error::Event(format!("invalid event kind: {kind}")))
    }
}

impl EventBus {
    pub fn new(db: Db, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { db, tx }
    }

    /// 在呼叫者的交易內記錄事件（與業務資料同交易）；commit 後務必呼叫 [`EventBus::announce`]。
    pub fn record(
        tx: &Transaction,
        kind: &str,
        version: u16,
        payload: serde_json::Value,
    ) -> Result<EventEnvelope> {
        validate_kind(kind)?;
        tx.execute(
            "INSERT INTO events(kind, version, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            (kind, version, payload.to_string(), now()),
        )?;
        Ok(EventEnvelope {
            seq: tx.last_insert_rowid() as u64,
            kind: kind.into(),
            version,
            payload,
        })
    }

    /// 廣播已提交的事件。沒有訂閱者不算錯誤。
    pub fn announce(&self, ev: EventEnvelope) {
        let _ = self.tx.send(ev);
    }

    /// 單獨發一個事件（自己開交易）。
    pub fn publish(
        &self,
        kind: &str,
        version: u16,
        payload: serde_json::Value,
    ) -> Result<EventEnvelope> {
        let ev = self.db.with(|c| {
            let tx = c.transaction()?;
            let ev = Self::record(&tx, kind, version, payload)?;
            tx.commit()?;
            Ok(ev)
        })?;
        self.announce(ev.clone());
        Ok(ev)
    }

    /// 讀 seq 之後的事件（SSE 斷線補送、訂閱者落後補讀）。
    pub fn since(&self, seq: u64, limit: usize) -> Result<Vec<EventEnvelope>> {
        self.db.with(|c| {
            let mut st = c.prepare("SELECT seq, kind, version, payload FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
            let rows = st.query_map((seq as i64, limit as i64), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, u16>(2)?, r.get::<_, String>(3)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (seq, kind, version, payload) = row?;
                out.push(EventEnvelope { seq: seq as u64, kind, version, payload: serde_json::from_str(&payload)? });
            }
            Ok(out)
        })
    }

    pub fn subscribe(&self) -> Subscription {
        Subscription {
            bus: self.clone(),
            rx: self.tx.subscribe(),
            last: None,
            backlog: Default::default(),
        }
    }
}

/// 不漏事件的訂閱：依序號遞增交付，落後時從資料庫補讀。
pub struct Subscription {
    bus: EventBus,
    rx: broadcast::Receiver<EventEnvelope>,
    last: Option<u64>,
    backlog: std::collections::VecDeque<EventEnvelope>,
}

impl Subscription {
    /// 從指定序號之後開始（先補讀歷史）。
    pub fn from_seq(mut self, seq: u64) -> Self {
        self.last = Some(seq);
        self
    }

    pub async fn next(&mut self) -> Result<EventEnvelope> {
        loop {
            if let Some(ev) = self.backlog.pop_front() {
                self.last = Some(ev.seq);
                return Ok(ev);
            }
            if let Some(last) = self.last {
                let missed = self.bus.since(last, 500)?;
                if !missed.is_empty() {
                    self.backlog.extend(missed);
                    continue;
                }
            }
            match self.rx.recv().await {
                Ok(ev) => {
                    if self.last.is_some_and(|l| ev.seq <= l) {
                        continue;
                    }
                    if self.last.is_some_and(|l| ev.seq > l + 1) {
                        continue; // 有缺口：下一圈從資料庫補讀
                    }
                    self.last = Some(ev.seq);
                    return Ok(ev);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if self.last.is_none() {
                        self.last = Some(0);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(Error::Event("bus closed".into()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kind_validation() {
        assert!(validate_kind("chat.message.created").is_ok());
        assert!(validate_kind("chat").is_err());
        assert!(validate_kind("Chat.x").is_err());
        assert!(validate_kind("chat..x").is_err());
    }

    #[tokio::test]
    async fn publish_persists_and_delivers_in_order() {
        let bus = EventBus::new(Db::open_in_memory().unwrap(), 16);
        let mut sub = bus.subscribe();
        for i in 0..3 {
            bus.publish("demo.tick", 1, json!({"i": i})).unwrap();
        }
        for i in 0..3 {
            assert_eq!(sub.next().await.unwrap().payload["i"], i);
        }
        assert_eq!(bus.since(0, 10).unwrap().len(), 3);
    }

    #[tokio::test]
    async fn lagged_subscriber_recovers_from_db() {
        let bus = EventBus::new(Db::open_in_memory().unwrap(), 2);
        let mut sub = bus.subscribe().from_seq(0);
        for i in 0..10 {
            bus.publish("demo.tick", 1, json!({"i": i})).unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..10 {
            got.push(sub.next().await.unwrap().payload["i"].as_i64().unwrap());
        }
        assert_eq!(got, (0..10).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn replay_from_seq() {
        let bus = EventBus::new(Db::open_in_memory().unwrap(), 16);
        for i in 0..5 {
            bus.publish("demo.tick", 1, json!({"i": i})).unwrap();
        }
        let mut sub = bus.subscribe().from_seq(3);
        assert_eq!(sub.next().await.unwrap().seq, 4);
        assert_eq!(sub.next().await.unwrap().seq, 5);
    }

    #[test]
    fn record_rolls_back_with_transaction() {
        let db = Db::open_in_memory().unwrap();
        let bus = EventBus::new(db.clone(), 4);
        db.with(|c| {
            let tx = c.transaction()?;
            EventBus::record(&tx, "demo.tick", 1, json!({}))?;
            drop(tx); // 未 commit
            Ok(())
        })
        .unwrap();
        assert!(bus.since(0, 10).unwrap().is_empty());
    }
}
