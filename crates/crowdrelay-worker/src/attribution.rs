//! Attribution worker — processes pending attribution requests from
//! the outbox and writes credited entries to the credit ledger.
//!
//! When a measurement completes, an `AttributionRequested` event is
//! enqueued in `viryaos_attribution_requests`. This worker calls
//! `process_attribution_batch` which claims pending requests, discovers
//! competing actions, runs the `ProportionalCreditAllocator`, and writes
//! the result to `viryaos_fan_credit_ledger`. The write is idempotent
//! on (measurement_id, attribution_version).
//!
//! The worker is decoupled from measurement completion — if it crashes
//! after the measurement transaction commits, the attribution request
//! is still pending and will be retried on the next poll cycle.

use std::time::Duration;

use crowdrelay_application::{RepositoryError, autopilot::AutopilotDecisionRepository};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};

const ATTRIBUTION_BATCH_SIZE: u32 = 16;

#[derive(Clone, Debug)]
pub struct AttributionWorker {
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
}

impl AttributionWorker {
    #[must_use]
    pub fn new(
        repository: PostgresAutopilotRepository,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
    ) -> Self {
        Self {
            repository,
            workspace_id,
            poll_interval,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticks = interval(self.poll_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    if let Err(e) = self.process_batch().await {
                        tracing::warn!(error = %e, "attribution worker batch failed");
                    }
                }
                result = shutdown.changed() => {
                    if result.is_ok() && *shutdown.borrow() {
                        tracing::info!("attribution worker shutting down");
                        break;
                    }
                }
            }
        }
    }

    async fn process_batch(&self) -> Result<(), RepositoryError> {
        let processed = self
            .repository
            .process_attribution_batch(self.workspace_id, ATTRIBUTION_BATCH_SIZE)
            .await?;
        if processed > 0 {
            tracing::info!(processed, "attribution worker processed batch");
        }
        Ok(())
    }
}
