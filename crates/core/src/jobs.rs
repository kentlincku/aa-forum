//! 背景工作：feature 註冊定時工作，由 supervisor 監督執行。

use crate::feature::Core;
use crate::supervisor::{self, RestartPolicy, ServiceState};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

pub type JobFn =
    Arc<dyn Fn(Arc<Core>) -> Pin<Box<dyn Future<Output = crate::Result<()>> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct Job {
    pub name: &'static str,
    pub every: Duration,
    pub run: JobFn,
}

impl Job {
    pub fn every<F, Fut>(name: &'static str, every: Duration, f: F) -> Self
    where
        F: Fn(Arc<Core>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<()>> + Send + 'static,
    {
        Self {
            name,
            every,
            run: Arc::new(move |c| Box::pin(f(c))),
        }
    }
}

pub(crate) fn start(job: Job, core: Arc<Core>) -> watch::Receiver<ServiceState> {
    supervisor::spawn(job.name, RestartPolicy::default(), move || {
        let (job, core) = (job.clone(), core.clone());
        Box::pin(async move {
            let mut tick = tokio::time::interval(job.every);
            loop {
                tick.tick().await;
                (job.run)(core.clone()).await?;
            }
        })
    })
}
