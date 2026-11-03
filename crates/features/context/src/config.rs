use aaf_core::{
    config::{Config, ConfigKey},
    Error, Result,
};
pub const CONFIG: &[ConfigKey] = &[
    ConfigKey {
        key: "AAF_CONTEXT_ENABLED",
        default: "1",
        description: "Durable context recording/recovery (0 or 1)",
    },
    ConfigKey {
        key: "AAF_CONTEXT_BYTES",
        default: "16384",
        description: "Maximum continuation bytes; 0 disables injection",
    },
    ConfigKey {
        key: "AAF_CONTEXT_TOKENS",
        default: "8192",
        description: "Conservative token ceiling (UTF-8 byte upper bound); 0 disables injection",
    },
    ConfigKey {
        key: "AAF_CONTEXT_ROTATE_BYTES",
        default: "0",
        description: "Observed session text bytes before rotation; 0 disables rotation",
    },
    ConfigKey {
        key: "AAF_CONTEXT_ROTATE_PCT",
        default: "auto",
        description: "Replace the session when agent-reported context use reaches this percent: auto (Claude 60, Codex 70, others 65), off, or 1..99",
    },
    ConfigKey {
        key: "AAF_CONTEXT_PAGE_BYTES",
        default: "16384",
        description: "Source page bytes (4..1048576)",
    },
];
#[derive(Clone)]
pub struct Settings {
    pub enabled: bool,
    pub budget: usize,
    pub rotate_bytes: usize,
    /// None＝依 agent 預設（auto）；Some(0)＝關閉；Some(1..=99)＝固定門檻
    pub rotate_pct: Option<u8>,
    pub page_bytes: usize,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            budget: 8192,
            rotate_bytes: 0,
            rotate_pct: None,
            page_bytes: 16384,
        }
    }
}
impl Settings {
    pub fn parse(c: &Config) -> Result<Self> {
        let enabled: usize = c.parse("AAF_CONTEXT_ENABLED")?;
        let bytes: usize = c.parse("AAF_CONTEXT_BYTES")?;
        let tokens: usize = c.parse("AAF_CONTEXT_TOKENS")?;
        let rotate_bytes = c.parse("AAF_CONTEXT_ROTATE_BYTES")?;
        let page_bytes = c.parse("AAF_CONTEXT_PAGE_BYTES")?;
        let pct: String = c.parse("AAF_CONTEXT_ROTATE_PCT")?;
        let rotate_pct = match pct.trim() {
            "auto" => None,
            "off" => Some(0),
            n => match n.parse::<u8>() {
                Ok(v) if (1..=99).contains(&v) => Some(v),
                _ => {
                    return Err(Error::Config(
                        "AAF_CONTEXT_ROTATE_PCT: auto, off, or 1..99".into(),
                    ))
                }
            },
        };
        let budget = bytes.min(tokens);
        if enabled > 1
            || (budget != 0 && !(1024..=1_048_576).contains(&budget))
            || !(4..=1_048_576).contains(&page_bytes)
        {
            return Err(Error::Config(
                "invalid context limits: enable 0/1, injection 0 or 1024..1048576, page 4..1048576"
                    .into(),
            ));
        }
        Ok(Self {
            enabled: enabled == 1,
            budget,
            rotate_bytes,
            rotate_pct,
            page_bytes,
        })
    }
}
