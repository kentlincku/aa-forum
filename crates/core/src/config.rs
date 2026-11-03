//! 設定：分層 預設值 → instance `.aaf.env`（KEY=VALUE，不執行）→ 環境變數。
//! 只接受已宣告的 key；未宣告的 key 讀取時報錯，檔案中出現則忽略並回報。

use crate::{Error, Result};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Copy)]
pub struct ConfigKey {
    /// 大寫，`AAF_` 開頭，例如 `AAF_TRASH_RETENTION_DAYS`。
    pub key: &'static str,
    pub default: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct Config {
    declared: HashMap<&'static str, ConfigKey>,
    values: HashMap<String, String>,
    /// 檔案中出現但未宣告的 key（給 doctor 告警）。
    pub unknown: Vec<String>,
}

impl Config {
    pub fn declare(&mut self, k: &ConfigKey) -> Result<()> {
        if !k.key.starts_with("AAF_")
            || k.key
                .chars()
                .any(|c| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        {
            return Err(Error::Config(format!("invalid key name: {}", k.key)));
        }
        if self.declared.insert(k.key, *k).is_some() {
            return Err(Error::Config(format!("key declared twice: {}", k.key)));
        }
        Ok(())
    }

    /// 依序疊上檔案與環境變數（後者優先）。
    pub fn load(
        &mut self,
        env_file: Option<&Path>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<()> {
        if let Some(p) = env_file.filter(|p| p.exists()) {
            let text = std::fs::read_to_string(p)
                .map_err(|e| Error::Config(format!("{}: {e}", p.display())))?;
            for line in text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
            {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                let (k, v) = (k.trim(), v.trim().trim_matches('"'));
                if self.declared.contains_key(k) {
                    self.values.insert(k.into(), v.into());
                } else {
                    self.unknown.push(k.into());
                }
            }
        }
        for (k, v) in env {
            if self.declared.contains_key(k.as_str()) {
                self.values.insert(k, v);
            }
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<&str> {
        let d = self
            .declared
            .get(key)
            .ok_or_else(|| Error::Config(format!("undeclared key: {key}")))?;
        Ok(self
            .values
            .get(key)
            .map(String::as_str)
            .unwrap_or(d.default))
    }

    pub fn parse<T: FromStr>(&self, key: &str) -> Result<T> {
        let v = self.get(key)?;
        v.parse()
            .map_err(|_| Error::Config(format!("{key}: cannot parse '{v}'")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAYS: ConfigKey = ConfigKey {
        key: "AAF_TRASH_RETENTION_DAYS",
        default: "30",
        description: "垃圾桶保留天數，0＝不自動清空",
    };

    #[test]
    fn layering_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(".aaf.env");
        std::fs::write(&f, "# c\nAAF_TRASH_RETENTION_DAYS=7\nAAF_NOPE=1\n").unwrap();
        let mut c = Config::default();
        c.declare(&DAYS).unwrap();
        assert_eq!(c.parse::<u32>("AAF_TRASH_RETENTION_DAYS").unwrap(), 30);
        c.load(Some(&f), []).unwrap();
        assert_eq!(c.parse::<u32>("AAF_TRASH_RETENTION_DAYS").unwrap(), 7);
        assert_eq!(c.unknown, vec!["AAF_NOPE"]);
        c.load(None, [("AAF_TRASH_RETENTION_DAYS".into(), "0".into())])
            .unwrap();
        assert_eq!(c.get("AAF_TRASH_RETENTION_DAYS").unwrap(), "0");
        assert!(c.get("AAF_NOPE").is_err());
        assert!(c.declare(&DAYS).is_err());
        assert!(c.declare(&ConfigKey { key: "bad", ..DAYS }).is_err());
    }
}
