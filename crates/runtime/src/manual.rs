//! manual：代表人類成員，不會被喚醒。

use crate::driver::*;
use async_trait::async_trait;
use tokio::sync::broadcast;

pub struct ManualDriver {
    tx: broadcast::Sender<DriverEvent>,
}

impl Default for ManualDriver {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(1).0,
        }
    }
}

#[async_trait]
impl Driver for ManualDriver {
    fn kind(&self) -> &'static str {
        "manual"
    }
    async fn start(&self) -> DResult<()> {
        Ok(())
    }
    async fn prompt(&self, _: &str) -> DResult<TurnResult> {
        Err(DriverError::Unsupported("manual roles are not prompted"))
    }
    async fn cancel(&self) -> DResult<()> {
        Ok(())
    }
    async fn respond_permission(&self, _: u64, _: PermissionDecision) -> DResult<()> {
        Err(DriverError::Unsupported("manual"))
    }
    fn status(&self) -> AgentStatus {
        AgentStatus::Offline
    }
    fn events(&self) -> broadcast::Receiver<DriverEvent> {
        self.tx.subscribe()
    }
    async fn stop(&self) -> DResult<()> {
        Ok(())
    }
}
