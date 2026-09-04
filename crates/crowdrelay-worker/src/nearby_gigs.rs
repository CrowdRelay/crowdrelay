//! Emits the nearby-show notifications a fan asked for.
//!
//! `/v1/internal/nearby-gigs/emit-due` has existed, been routed and been
//! covered by tests without a single caller: no worker loop, no n8n node, no
//! cron, no `crowdrelayctl` command. The fan-facing toggle stored a preference,
//! the delivery side was wired down to the edge route for
//! `fan.nearby_concert_available`, and the event that drives it was never
//! produced. This is the only automatic reason an installed app reopens
//! itself, so the whole re-engagement loop was inert.
//!
//! It lives here rather than in n8n because the schedule belongs next to the
//! other first-party loops, under the same leadership election, and because
//! the endpoint's authority is internal: routing it back out through HTTP to
//! call ourselves adds a credential and a hop for nothing.
//!
//! The underlying statement is idempotent -- it skips pairs already notified
//! and the insert guards the race -- so a missed tick costs latency, never a
//! duplicate, and a double run is harmless.

use std::time::Duration;

use sqlx::PgPool;
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::mobile_fan::PostgresMobileFanRepository;

/// Shows are announced days ahead, not seconds, and the statement only ever
/// reports pairs it has not already reported. Polling harder would buy nothing
/// and each tick still scans the candidate set.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Error)]
pub enum NearbyGigsError {
    #[error("nearby gig emission failed: {0}")]
    Store(String),
}

pub struct NearbyGigScheduler {
    database: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
}

impl NearbyGigScheduler {
    #[must_use]
    pub fn new(
        database: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            database,
            workspace_id,
            poll_interval,
            operation_timeout,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                _ = ticker.tick() => {
                    match timeout(self.operation_timeout, self.emit_due()).await {
                        Ok(Ok((queued, pushed))) if queued > 0 || pushed > 0 => {
                            tracing::info!(queued, pushed, "nearby shows announced");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(error = ?error, "nearby show emission failed");
                        }
                        Err(_) => {
                            // The statement runs under the operation timeout and
                            // only considers pairs it has not announced, so a
                            // timeout means the candidate set outgrew the budget
                            // rather than that anything is stuck. Worth saying
                            // out loud: silence here is indistinguishable from a
                            // quiet week.
                            tracing::warn!("nearby show emission timed out");
                        }
                    }
                }
            }
        }
    }

    async fn emit_due(&self) -> Result<(i64, i64), NearbyGigsError> {
        // Push is a separate consent-and-delivery decision from mail, and the
        // flag defaults off, so read it per run rather than caching it: turning
        // push on should not need a worker restart.
        let push_enabled = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT COALESCE(
                bool_or(enabled) FILTER (WHERE key = 'push_delivery_enabled'),
                false
            )
            FROM ecosystem_feature_flags
            WHERE workspace_id = $1 AND key = 'push_delivery_enabled'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_one(&self.database)
        .await
        .map_err(|error| NearbyGigsError::Store(error.to_string()))?;

        let request_id = format!("nearby-gigs:{}", Uuid::now_v7().simple());
        PostgresMobileFanRepository::new(
            self.database.clone(),
            self.workspace_id,
            self.operation_timeout,
        )
        .emit_due_nearby_gigs(Some(&request_id), push_enabled)
        .await
        .map_err(|error| NearbyGigsError::Store(error.to_string()))
    }
}
