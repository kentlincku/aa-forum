//! Best-effort credential minimization before durable observation, not a secret detector.
use serde_json::Value;
use std::sync::OnceLock;

pub fn credential_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace('-', "_");
    [
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "credential",
        "private_key",
        "access_key",
    ]
    .iter()
    .any(|s| k.contains(s))
}

#[derive(Default, Clone)]
pub struct Sanitizer {
    known: Vec<String>,
}
impl Sanitizer {
    pub fn from_env(env: &[(String, String)]) -> Self {
        let mut known: Vec<_> = env
            .iter()
            .filter(|(k, v)| credential_key(k) && !v.is_empty())
            .map(|(_, v)| v.clone())
            .collect();
        known.sort_by_key(|s| std::cmp::Reverse(s.len()));
        known.dedup();
        Self { known }
    }
    pub fn text(&self, text: &str) -> String {
        static ASSIGN: OnceLock<regex::Regex> = OnceLock::new();
        static BEARER: OnceLock<regex::Regex> = OnceLock::new();
        static PREFIX: OnceLock<regex::Regex> = OnceLock::new();
        static USERINFO: OnceLock<regex::Regex> = OnceLock::new();
        static PEM: OnceLock<regex::Regex> = OnceLock::new();
        let mut text = text.to_string();
        for value in &self.known {
            text = text.replace(value, "[REDACTED]");
        }
        let bearer = BEARER
            .get_or_init(|| regex::Regex::new(r#"(?i)\b(?:Bearer|Basic)\s+[^\s"',;}]+"#).unwrap());
        text = bearer.replace_all(&text, "[REDACTED]").into_owned();
        let userinfo =
            USERINFO.get_or_init(|| regex::Regex::new(r"(https?://)[^/@\s]+:[^/@\s]+@").unwrap());
        text = userinfo.replace_all(&text, "${1}[REDACTED]@").into_owned();
        let pem = PEM.get_or_init(|| {
            regex::Regex::new(
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            )
            .unwrap()
        });
        text = pem.replace_all(&text, "[REDACTED]").into_owned();
        let assignment = ASSIGN.get_or_init(|| regex::Regex::new(r#"(?i)([\w-]*(?:token|secret|password|passwd|api_key|apikey|authorization|cookie|credential|private_key|access_key)[\w-]*[\"']?\s*[:=]\s*)(?:\"[^\"]*\"|'[^']*'|[^\s,;}]+)"#).unwrap());
        text = assignment.replace_all(&text, "${1}[REDACTED]").into_owned();
        let prefix = PREFIX.get_or_init(|| {
            regex::Regex::new(r"\b(?:aafr_|sk-|ghp_|github_pat_)[A-Za-z0-9_-]+").unwrap()
        });
        prefix.replace_all(&text, "[REDACTED]").into_owned()
    }
    pub fn messages(
        &self,
        messages: &[crate::dispatcher::InboxMessage],
    ) -> Vec<crate::dispatcher::InboxMessage> {
        messages
            .iter()
            .cloned()
            .map(|mut m| {
                m.body = self.text(&m.body);
                m.from = self.text(&m.from);
                m
            })
            .collect()
    }
    pub fn result(
        &self,
        result: &crate::driver::DResult<crate::driver::TurnResult>,
    ) -> crate::driver::DResult<crate::driver::TurnResult> {
        match result {
            Ok(r) => Ok(crate::driver::TurnResult {
                text: self.text(&r.text),
                stop_reason: r.stop_reason.clone(),
            }),
            Err(e) => Err(crate::driver::DriverError::Protocol(
                self.text(&e.to_string()),
            )),
        }
    }
    pub fn value(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if credential_key(key) {
                        *value = Value::String("[REDACTED]".into());
                    } else {
                        self.value(value);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.value(value);
                }
            }
            Value::String(s) => *s = self.text(s),
            _ => {}
        }
    }
}
