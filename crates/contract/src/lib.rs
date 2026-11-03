//! 對外契約（v1）：事件、API DTO、driver 契約。只新增欄位，不改既有語意。

use serde::{Deserialize, Serialize};

pub const CONTRACT_VERSION: &str = "v1";

/// 事件信封：所有狀態變化都以事件表達。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// 單調遞增序號，SSE 斷線補送用。
    pub seq: u64,
    /// 事件種類，例如 `chat.message.created`；由各 feature 自訂命名空間。
    pub kind: String,
    /// 事件 payload 的 schema 版本。
    pub version: u16,
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips_as_json() {
        let e = EventEnvelope {
            seq: 1,
            kind: "system.started".into(),
            version: 1,
            payload: serde_json::json!({}),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<EventEnvelope>(&s).unwrap(), e);
    }
}
