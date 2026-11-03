//! 權限：功能宣告能力（capability）與預設授權的 rank；未宣告的能力一律拒絕（fail closed）。

use crate::{Error, Result};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rank {
    /// 擁有者：擁有所有能力。
    Owner,
    Human,
    Lead,
    Worker,
}

#[derive(Debug, Clone, Copy)]
pub struct CapabilityDef {
    /// `<feature>.<object>.<action>`，例如 `chat.room.delete`。
    pub id: &'static str,
    pub description: &'static str,
    pub default_ranks: &'static [Rank],
}

#[derive(Debug, Default, Clone)]
pub struct Policy {
    grants: HashMap<&'static str, HashSet<Rank>>,
    descriptions: HashMap<&'static str, &'static str>,
}

impl Policy {
    pub fn declare(&mut self, def: &CapabilityDef) -> Result<()> {
        if self.grants.contains_key(def.id) {
            return Err(Error::Feature(format!(
                "capability declared twice: {}",
                def.id
            )));
        }
        self.grants
            .insert(def.id, def.default_ranks.iter().copied().collect());
        self.descriptions.insert(def.id, def.description);
        Ok(())
    }

    pub fn check(&self, rank: Rank, capability: &str) -> Result<()> {
        if rank == Rank::Owner && self.grants.contains_key(capability) {
            return Ok(());
        }
        match self.grants.get(capability) {
            Some(r) if r.contains(&rank) => Ok(()),
            Some(_) => Err(Error::Denied(format!("{rank:?} lacks {capability}"))),
            None => Err(Error::Denied(format!("undeclared capability {capability}"))),
        }
    }

    /// 管理介面用：列出所有能力與說明。
    pub fn list(&self) -> Vec<(&'static str, &'static str)> {
        let mut v: Vec<_> = self.descriptions.iter().map(|(k, d)| (*k, *d)).collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEL: CapabilityDef = CapabilityDef {
        id: "chat.room.delete",
        description: "永久刪除群",
        default_ranks: &[],
    };
    const POST: CapabilityDef = CapabilityDef {
        id: "chat.message.post",
        description: "發訊息",
        default_ranks: &[Rank::Human, Rank::Lead, Rank::Worker],
    };

    #[test]
    fn owner_only_and_fail_closed() {
        let mut p = Policy::default();
        p.declare(&DEL).unwrap();
        p.declare(&POST).unwrap();
        assert!(p.check(Rank::Owner, "chat.room.delete").is_ok());
        assert!(p.check(Rank::Lead, "chat.room.delete").is_err());
        assert!(p.check(Rank::Worker, "chat.message.post").is_ok());
        assert!(p.check(Rank::Owner, "nope.x.y").is_err());
        assert!(p.declare(&DEL).is_err());
        assert_eq!(p.list().len(), 2);
    }
}
