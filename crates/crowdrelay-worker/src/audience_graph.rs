//! Audience Graph maintenance sweep.
//!
//! Deterministic decay for the discovery pipeline: relationships that went
//! silent fall dormant, and the starvation signal (discovered-but-never-
//! researched places) is counted so operators and future autopilot contexts
//! can see the supply state without querying tables by hand.
//!
//! This pass never contacts anyone. Contact is an operator action through the
//! admin surface; the sweep only keeps the pipeline honest about time.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};

/// A relationship with no recorded action for this long stops counting as a
/// live negotiation. Chosen well above a festival season's quiet stretch so
/// normal radio silence does not retire pipelines mid-negotiation.
const DEFAULT_DORMANT_AFTER_DAYS: i32 = 45;
/// Batch cap per pass, mirroring the retention worker's bounded-sweep style.
const DECAY_BATCH: i64 = 500;

#[derive(Debug)]
pub struct AudienceGraphSweeper {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
    dormant_after_days: i32,
}

impl AudienceGraphSweeper {
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            pool,
            workspace_id,
            poll_interval,
            operation_timeout,
            dormant_after_days: DEFAULT_DORMANT_AFTER_DAYS,
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
                    match timeout(self.operation_timeout, self.run_once()).await {
                        Ok(Ok(report)) if report.decayed > 0 || report.discovered_never_researched > 0 => {
                            tracing::info!(
                                decayed = report.decayed,
                                undiscovered = report.discovered_never_researched,
                                contactable = report.contactable_places,
                                "audience graph sweep retired silent pipelines"
                            );
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(error = %error, "audience graph sweep failed")
                        }
                        Err(_) => tracing::warn!("audience graph sweep timed out"),
                    }
                }
            }
        }
    }

    async fn run_once(&self) -> Result<SweepReport, CrowdError> {
        let repository = crowdrelay_infra::audience_graph::PostgresAudienceGraphRepository::new(
            self.pool.clone(),
        );
        let decayed = repository
            .decay_dormant(
                self.workspace_id.into_uuid(),
                time::Duration::days(i64::from(self.dormant_after_days)),
                DECAY_BATCH,
            )
            .await
            .map_err(CrowdError::Graph)?;
        let supply = sqlx::query_as::<_, (i64, i64)>(
            r#"
            SELECT
                count(*) FILTER (
                    WHERE outreach.stage = 'discovered'
                      AND place.created_at < now() - interval '14 days'
                )::bigint,
                count(*) FILTER (
                    WHERE outreach.stage IN ('researched', 'replied', 'negotiating')
                      AND outreach.next_eligible_at <= now()
                )::bigint
            FROM discovery_outreach AS outreach
            JOIN discovery_places AS place ON place.id = outreach.place_id
            WHERE outreach.workspace_id = $1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(CrowdError::Sqlx)?;
        Ok(SweepReport {
            decayed,
            discovered_never_researched: supply.0,
            contactable_places: supply.1,
        })
    }
}

/// The sweep reports two different persistence failures; both are logged and
/// never fatal to the process.
#[derive(Debug, thiserror::Error)]
enum CrowdError {
    #[error("audience graph database operation failed")]
    Sqlx(#[from] sqlx::Error),
    #[error("audience graph pipeline error")]
    Graph(crowdrelay_infra::audience_graph::AudienceGraphError),
}

struct SweepReport {
    decayed: u64,
    discovered_never_researched: i64,
    contactable_places: i64,
}
