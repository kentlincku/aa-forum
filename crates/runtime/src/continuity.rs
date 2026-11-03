//! Optional durable-context extension. Persistence policy belongs to a feature.
use crate::dispatcher::{InboxMessage, Lane};
use crate::driver::{DResult, TurnResult};
use aaf_core::Result;

#[derive(Default)]
pub struct PreparedContext {
    pub text: String,
    pub cursor: i64,
    pub restoring: bool,
    pub input_bytes: usize,
    pub attempt: i64,
}

pub trait Continuity: Send + Sync {
    fn enabled(&self) -> bool {
        true
    }
    /// Admission check after restart; unresolved effects require an explicit operator recovery.
    fn fault(&self, _lane: &Lane) -> Result<Option<String>> {
        Ok(None)
    }
    fn failed(&self, _lane: &Lane, _error: &str) -> Result<()> {
        Ok(())
    }
    fn recover(&self, _lane: &Lane) -> Result<()> {
        Ok(())
    }
    fn rotate(&self, _lane: &Lane) -> Result<bool> {
        Ok(false)
    }
    /// 回合結束後 agent 回報的上下文用量（ACP usage_update）；rotate 依它判斷要不要換 session。
    fn usage(&self, _lane: &Lane, _usage: &crate::driver::ContextUsage) -> Result<()> {
        Ok(())
    }
    /// 換 session 前 agent 自己寫的交接；新 session 的 continuation 會先放它。
    fn handoff(&self, _lane: &Lane, _text: &str) -> Result<()> {
        Ok(())
    }
    fn observed(&self, _lane: &Lane, _seq: u64, _event: &crate::driver::DriverEvent) -> Result<()> {
        Ok(())
    }
    /// Persist the actual new/load outcome before the native session id is saved.
    fn started(&self, _lane: &Lane, _session: Option<&str>, _loaded: bool) -> Result<()> {
        Ok(())
    }
    /// Called before dispatch. An error must leave the messages queued.
    fn prepare(
        &self,
        lane: &Lane,
        recover: bool,
        messages: &[InboxMessage],
    ) -> Result<PreparedContext>;
    /// Commit the checkpoint before acknowledging messages or clearing recovery pending.
    fn complete(
        &self,
        lane: &Lane,
        prepared: &PreparedContext,
        result: &DResult<TurnResult>,
    ) -> Result<()>;
}
pub(crate) type Flush = tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<Result<()>>>;

/// A clean close has already drained the recorder. Completion may race with that close.
pub(crate) async fn flush_events(
    flush: &Flush,
    closed: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    if closed.load(Ordering::SeqCst) {
        return Ok(());
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let result = if flush.send(tx).await.is_ok() {
        rx.await.ok()
    } else {
        None
    };
    match result {
        Some(r) => r,
        None if closed.load(Ordering::SeqCst) => Ok(()),
        None => Err(aaf_core::Error::Other(
            "context event recorder closed without a successful drain".into(),
        )),
    }
}
