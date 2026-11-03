//! 通用檢查掛點 hooks（源自先前的 Python 版）。
//!
//! 規則：`<hooks 目錄>/*` 中可執行的一般檔案（不跟 symlink；`_`、`.` 開頭略過），依檔名排序執行。
//! 事件以 JSON 從 stdin 傳入，環境變數 `AAF_HOOK_PHASE` 為 pre／post／wake／turn_end。
//! 規則印到 stdout 的文字（去頭尾空白）就是結果：
//! - pre：非空＝擋下，文字是給發文者看的原因；第一個擋下的生效。
//! - post／turn_end：非空＝提醒，下次叫醒該角色時附在提示最前面（一次後清除、同內容去重）。
//! - wake：非空＝附在這一輪提示最前面。
//!
//! fail-open：規則逾時（預設 5 秒）、非 0 結束、啟動失敗都放行，錯誤寫進 `var/hooks.log`；
//! 不能因為規則寫壞讓整個團隊無法說話。規則只拿到最小環境（PATH、HOME、AAF_HOME），不含任何 token。
//!
//! 事件欄位：type（chat.post／task.post／wake／turn_end）、sender、rank、room、body、reply_to、
//! role、unread、session_new、ok、error、text。

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const OUTPUT_LIMIT: usize = 4096;

#[derive(Debug, Clone)]
pub struct Hooks {
    dir: PathBuf,
    var: PathBuf,
    instance: PathBuf,
    timeout: Duration,
}

impl Hooks {
    pub fn new(dir: impl Into<PathBuf>, instance: impl Into<PathBuf>) -> Self {
        let instance = instance.into();
        Self {
            dir: dir.into(),
            var: instance.join("var"),
            instance,
            timeout: Duration::from_secs(5),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 目前的規則檔（依檔名排序）。
    pub fn rules(&self) -> Vec<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let mut v: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                !n.starts_with('_') && !n.starts_with('.') && !n.ends_with('~')
            })
            .filter(|e| {
                std::fs::symlink_metadata(e.path())
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .map(|e| e.path())
            .collect();
        v.sort();
        v
    }

    pub fn log(&self, text: &str) {
        use std::io::Write;
        let _ = std::fs::create_dir_all(&self.var);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.var.join("hooks.log"))
        {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "{ts} {}", text.trim_end());
        }
    }

    async fn run_one(&self, rule: &Path, phase: &str, event: &Value) -> Option<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let name = rule
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut cmd = tokio::process::Command::new(rule);
        cmd.env_clear()
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("AAF_HOME", &self.instance)
            .env("AAF_HOOK_PHASE", phase)
            .current_dir(&self.instance)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.log(&format!("{phase} {name}: cannot start ({e}); allowed"));
                return None;
            }
        };
        let input = event.to_string();
        let mut stdin = child.stdin.take()?;
        let mut stdout = child.stdout.take()?;
        let mut stderr = child.stderr.take()?;
        let work = async move {
            let _ = stdin.write_all(input.as_bytes()).await;
            drop(stdin);
            let mut out = Vec::new();
            let mut err = Vec::new();
            let _ = (&mut stdout)
                .take(OUTPUT_LIMIT as u64 + 1)
                .read_to_end(&mut out)
                .await;
            let _ = (&mut stderr)
                .take(OUTPUT_LIMIT as u64)
                .read_to_end(&mut err)
                .await;
            let status = child.wait().await;
            (status, out, err)
        };
        match tokio::time::timeout(self.timeout, work).await {
            Err(_) => {
                self.log(&format!(
                    "{phase} {name}: timed out after {:?}; allowed",
                    self.timeout
                ));
                None
            }
            Ok((Err(e), _, _)) => {
                self.log(&format!("{phase} {name}: {e}; allowed"));
                None
            }
            Ok((Ok(st), _, err)) if !st.success() => {
                self.log(&format!(
                    "{phase} {name}: exited with {st}; allowed\n{}",
                    String::from_utf8_lossy(&err)
                ));
                None
            }
            Ok((Ok(_), out, _)) => {
                let mut s = String::from_utf8_lossy(&out[..out.len().min(OUTPUT_LIMIT)])
                    .trim()
                    .to_string();
                if out.len() > OUTPUT_LIMIT {
                    s.push('…');
                }
                (!s.is_empty()).then_some(s)
            }
        }
    }

    /// pre：回 Some(原因) 表示擋下。
    pub async fn pre(&self, event: &Value) -> Option<String> {
        for r in self.rules() {
            if let Some(reason) = self.run_one(&r, "pre", event).await {
                let name = r
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.log(&format!(
                    "blocked {} {} by {name}: {}",
                    event["type"].as_str().unwrap_or("?"),
                    event["sender"].as_str().unwrap_or("?"),
                    reason.chars().take(200).collect::<String>()
                ));
                return Some(reason);
            }
        }
        None
    }

    async fn collect(&self, phase: &str, event: &Value) -> Vec<String> {
        let mut out = Vec::new();
        for r in self.rules() {
            if let Some(t) = self.run_one(&r, phase, event).await {
                out.push(t);
            }
        }
        out
    }

    /// post：已送出之後；提醒存給發送者（角色）下次叫醒。
    pub async fn post(&self, event: &Value) {
        let notes = self.collect("post", event).await;
        if let Some(who) = event["sender"].as_str() {
            self.add_notes(who, &notes);
        }
    }

    /// wake：回傳要附在這一輪提示最前面的文字（含先前累積的提醒）。
    pub async fn wake(&self, event: &Value) -> String {
        let role = event["role"].as_str().unwrap_or_default();
        let mut parts = self.take_notes(role);
        parts.extend(self.collect("wake", event).await);
        if parts.is_empty() {
            return String::new();
        }
        let mut s = String::from("[AA Forum rules]\n");
        for p in parts {
            s.push_str(&format!("- {p}\n"));
        }
        s.push('\n');
        s
    }

    /// turn_end：提醒存給該角色下次叫醒。
    pub async fn turn_end(&self, event: &Value) {
        let notes = self.collect("turn_end", event).await;
        if let Some(role) = event["role"].as_str() {
            self.add_notes(role, &notes);
        }
    }

    fn notes_file(&self, who: &str) -> Option<PathBuf> {
        crate::dispatcher::valid_role_name(who)
            .then(|| self.var.join("hook_notes").join(format!("{who}.jsonl")))
    }

    pub fn add_notes(&self, who: &str, notes: &[String]) {
        use std::io::Write;
        let Some(f) = self.notes_file(who).filter(|_| !notes.is_empty()) else {
            return;
        };
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        if let Ok(mut fh) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&f)
        {
            for n in notes {
                let _ = writeln!(fh, "{}", serde_json::json!({"note": n}));
            }
        }
    }

    /// 取出並清除提醒（同內容只留一次）。
    pub fn take_notes(&self, who: &str) -> Vec<String> {
        let Some(f) = self.notes_file(who) else {
            return vec![];
        };
        let Ok(text) = std::fs::read_to_string(&f) else {
            return vec![];
        };
        let _ = std::fs::remove_file(&f);
        let mut out: Vec<String> = Vec::new();
        for l in text.lines() {
            if let Some(n) = serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| v["note"].as_str().map(str::to_string))
            {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        out
    }
}
