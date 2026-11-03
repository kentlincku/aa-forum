//! command：每輪執行一次指令，提示走 stdin，stdout 即回覆。無模型測試路徑（如 echo agent）。

use crate::driver::*;
use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Mutex};

pub struct CommandDriver {
    command: Vec<String>,
    cwd: PathBuf,
    sig: Signals,
    child: Mutex<Option<tokio::process::Child>>,
    cancel: tokio::sync::Notify,
    env: Vec<(String, String)>,
    registry: Option<crate::process::Registry>,
}

impl CommandDriver {
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    pub fn with_registry(mut self, r: Option<crate::process::Registry>) -> Self {
        self.registry = r;
        self
    }

    pub fn new(command: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            command,
            cwd,
            sig: Signals::new(AgentStatus::Idle),
            child: Mutex::new(None),
            cancel: tokio::sync::Notify::new(),
            env: vec![],
            registry: None,
        }
    }
}

#[async_trait]
impl Driver for CommandDriver {
    fn kind(&self) -> &'static str {
        "command"
    }
    async fn start(&self) -> DResult<()> {
        if self.command.is_empty() {
            return Err(DriverError::Protocol("empty command".into()));
        }
        Ok(())
    }
    async fn prompt(&self, text: &str) -> DResult<TurnResult> {
        if self.sig.get() == AgentStatus::Working {
            return Err(DriverError::Busy);
        }
        self.sig.set(AgentStatus::Working);
        let res = async {
            let mut cmd = tokio::process::Command::new(&self.command[0]);
            cmd.args(&self.command[1..])
                .current_dir(&self.cwd)
                .envs(self.env.iter().cloned())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            crate::process::isolate(&mut cmd);
            let mut child = cmd.spawn()?;
            let pid = child.id();
            if let (Some(r), Some(pid)) = (&self.registry, pid) {
                let _ = r.record(pid, &self.command.join(" "));
            }
            let mut stdin = child.stdin.take().expect("piped");
            // 指令不讀 stdin 就先結束了（例如 `echo`）：EPIPE 不算失敗，照常讀它的輸出
            match stdin.write_all(text.as_bytes()).await {
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => return Err(e.into()),
                _ => {}
            }
            drop(stdin);
            let mut stdout = child.stdout.take().expect("piped");
            *self.child.lock().await = Some(child);
            let mut out = Vec::new();
            let read = tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut out);
            let cancelled = tokio::select! {
                r = read => { r?; false }
                _ = self.cancel.notified() => true,
            };
            let mut child = self.child.lock().await.take().expect("set above");
            if let (Some(r), Some(pid)) = (&self.registry, pid) {
                r.forget(pid);
            }
            if cancelled {
                if let Some(pid) = pid {
                    crate::process::kill_group(pid, std::time::Duration::from_secs(3)).await;
                }
                let _ = child.kill().await;
                return Ok(TurnResult {
                    text: String::new(),
                    stop_reason: "cancelled".into(),
                });
            }
            let st = child.wait().await?;
            let text = String::from_utf8_lossy(&out).trim_end().to_string();
            self.sig
                .emit(DriverEvent::MessageChunk { text: text.clone() });
            Ok(TurnResult {
                text,
                stop_reason: if st.success() {
                    "end_turn".into()
                } else {
                    format!("error: exit {st}")
                },
            })
        }
        .await;
        self.sig.set(AgentStatus::Idle);
        if let Ok(r) = &res {
            self.sig.emit(DriverEvent::TurnEnded {
                stop_reason: r.stop_reason.clone(),
            });
        }
        res
    }
    async fn cancel(&self) -> DResult<()> {
        if self.sig.get() == AgentStatus::Working {
            self.cancel.notify_one();
        }
        Ok(())
    }
    async fn respond_permission(&self, _: u64, _: PermissionDecision) -> DResult<()> {
        Err(DriverError::Unsupported(
            "command driver has no permission requests",
        ))
    }
    fn status(&self) -> AgentStatus {
        self.sig.get()
    }
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.sig.tx.subscribe()
    }
    async fn stop(&self) -> DResult<()> {
        self.cancel().await
    }
}
