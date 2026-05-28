use crate::{error::ExecutorError, provider::RuntimeProvider, registry::RuntimeRegistry};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct Executor {
    pub(crate) executor_id: String,
    pub(crate) server_url: String,
    pub(crate) provider: Box<dyn RuntimeProvider>,
    pub(crate) registry: RuntimeRegistry,
    pub(crate) health_check_interval: Duration,
    pub(crate) max_restarts: u32,
}

impl Executor {
    pub fn new(
        executor_id: String,
        server_url: String,
        provider: Box<dyn RuntimeProvider>,
    ) -> Self {
        Self {
            executor_id,
            server_url,
            provider,
            registry: RuntimeRegistry::new(),
            health_check_interval: Duration::from_secs(30),
            max_restarts: 3,
        }
    }

    pub fn with_health_check_interval(mut self, interval: Duration) -> Self {
        self.health_check_interval = interval;
        self
    }

    pub fn with_max_restarts(mut self, max: u32) -> Self {
        self.max_restarts = max;
        self
    }

    pub async fn run(self, _cancel: CancellationToken) -> Result<(), ExecutorError> {
        Ok(())
    }
}
