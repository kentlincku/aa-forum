//! Supervisor：服務 task 失敗或 panic 時退避重啟；在時間窗內失敗過多則放棄（熔斷），不拖垮其他服務。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub type BoxFut = Pin<Box<dyn Future<Output = crate::Result<()>> + Send>>;

#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    pub max_failures: usize,
    pub window: Duration,
    pub backoff: Duration,
}

impl Default for RestartPolicy {
    /// 與 Python 版相同：5 分鐘內 3 次失敗即放棄。
    fn default() -> Self {
        Self {
            max_failures: 3,
            window: Duration::from_secs(300),
            backoff: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Running {
        restarts: usize,
    },
    /// 正常結束（服務自己回 Ok）。
    Stopped,
    GaveUp {
        last_error: String,
    },
}

/// 啟動一個受監督的服務；回傳狀態觀察者。
pub fn spawn(
    name: &'static str,
    policy: RestartPolicy,
    factory: impl Fn() -> BoxFut + Send + Sync + 'static,
) -> watch::Receiver<ServiceState> {
    let (tx, rx) = watch::channel(ServiceState::Running { restarts: 0 });
    let factory = Arc::new(factory);
    tokio::spawn(async move {
        let mut failures: Vec<Instant> = Vec::new();
        let mut restarts = 0;
        loop {
            let err = match tokio::spawn((factory)()).await {
                Ok(Ok(())) => {
                    let _ = tx.send(ServiceState::Stopped);
                    return;
                }
                Ok(Err(e)) => e.to_string(),
                Err(join) => format!("panic: {join}"),
            };
            tracing::warn!(service = name, error = %err, "service failed");
            let now = Instant::now();
            failures.retain(|t| now.duration_since(*t) < policy.window);
            failures.push(now);
            if failures.len() >= policy.max_failures {
                tracing::error!(
                    service = name,
                    "giving up after {} failures",
                    failures.len()
                );
                let _ = tx.send(ServiceState::GaveUp { last_error: err });
                return;
            }
            tokio::time::sleep(policy.backoff).await;
            restarts += 1;
            let _ = tx.send(ServiceState::Running { restarts });
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fast() -> RestartPolicy {
        RestartPolicy {
            max_failures: 3,
            window: Duration::from_secs(60),
            backoff: Duration::from_millis(1),
        }
    }

    async fn settle(rx: &mut watch::Receiver<ServiceState>) -> ServiceState {
        loop {
            let s = rx.borrow().clone();
            if !matches!(s, ServiceState::Running { .. }) {
                return s;
            }
            rx.changed().await.unwrap();
        }
    }

    #[tokio::test]
    async fn restarts_then_succeeds() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let mut rx = spawn("flaky", fast(), move || {
            let n = n2.clone();
            Box::pin(async move {
                if n.fetch_add(1, Ordering::SeqCst) < 1 {
                    Err(crate::Error::Other("boom".into()))
                } else {
                    Ok(())
                }
            })
        });
        assert_eq!(settle(&mut rx).await, ServiceState::Stopped);
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn gives_up_and_survives_panics() {
        let mut rx = spawn("panicky", fast(), || Box::pin(async { panic!("kaboom") }));
        match settle(&mut rx).await {
            ServiceState::GaveUp { last_error } => assert!(last_error.contains("panic")),
            s => panic!("unexpected {s:?}"),
        }
    }
}
