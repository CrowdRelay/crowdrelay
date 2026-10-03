//! Finds the fans most likely to carry the band to a friend, before they have
//! referred anybody, and records them as Latarnik candidates.
//!
//! It reads first-party behaviour only (`latarnik_roles::load_fan_evidence`),
//! asks the pure evaluator what each fan's advocacy readiness warrants, and
//! writes one of two durable planning facts with the evidence frozen on it: a
//! `candidate` role for the deep path, or a person-keyed `personal_referral`
//! opportunity for the light path. It never contacts anyone — delivery remains
//! in the consented fan lifecycle — and it never changes a fan's status.

use std::time::Duration;

use crowdrelay_domain::latarnik_mission::choose_mission;
use crowdrelay_domain::{
    WorkspaceId,
    latarnik::{LatarnikMove, evaluate},
};
use crowdrelay_infra::latarnik_missions::{load_carriers, offer, settle};
use crowdrelay_infra::latarnik_roles::{
    LatarnikError, load_fan_evidence, record_candidate, record_referral_opportunity,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};

pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Fans read per pass; a larger fanbase is worked through over passes, oldest
/// fans first.
pub const FANS_PER_PASS: i64 = 1_000;

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error(transparent)]
    Latarnik(#[from] LatarnikError),
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct SweepReport {
    pub fans_read: u64,
    /// Roles created this pass.
    pub candidates_recorded: u64,
    /// Fans whose readiness is real but light: the one-referral ask, not the role.
    pub light_ask_ready: u64,
    /// New person-keyed referral opportunities created this pass.
    pub referral_opportunities_recorded: u64,
    /// Missions offered this pass to active Latarniks, in their own session.
    pub missions_offered: u64,
    /// Missions that completed (someone they brought arrived) this pass.
    pub missions_completed: u64,
    /// Open missions that ran out their time this pass.
    pub missions_expired: u64,
}

#[derive(Clone, Debug)]
pub struct LatarnikSweep {
    pool: PgPool,
    workspace_id: WorkspaceId,
    operation_timeout: Duration,
}

impl LatarnikSweep {
    #[must_use]
    pub fn new(pool: PgPool, workspace_id: WorkspaceId, operation_timeout: Duration) -> Self {
        Self {
            pool,
            workspace_id,
            operation_timeout,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    match timeout(self.operation_timeout * 6, self.run_once(OffsetDateTime::now_utc())).await {
                        Ok(Ok(report)) if report != SweepReport::default() => {
                            tracing::info!(?report, "latarnik sweep");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(%error, "latarnik sweep failed"),
                        Err(_) => tracing::warn!("latarnik sweep timed out"),
                    }
                }
            }
        }
    }

    /// One pass; public so tests drive the real queries with their own clock.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn run_once(&self, now: OffsetDateTime) -> Result<SweepReport, SweepError> {
        let ws = self.workspace_id.into_uuid();
        let mut report = SweepReport::default();
        for fan in load_fan_evidence(&self.pool, ws, now, FANS_PER_PASS).await? {
            report.fans_read += 1;
            match evaluate(&fan.evidence) {
                LatarnikMove::InviteToLatarnik => {
                    if record_candidate(&self.pool, ws, &fan.email, &fan.evidence, now)
                        .await?
                        .is_some()
                    {
                        report.candidates_recorded += 1;
                    }
                }
                LatarnikMove::AskForReferral => {
                    report.light_ask_ready += 1;
                    if record_referral_opportunity(&self.pool, ws, &fan.email, &fan.evidence, now)
                        .await?
                        .is_some()
                    {
                        report.referral_opportunities_recorded += 1;
                    }
                }
                LatarnikMove::None(_) => {}
            }
        }
        // Missions: close what has run its course first, so a mission that just
        // expired does not hold its Latarnik's single slot against a fresh one.
        let (completed, expired) = settle(&self.pool, ws, now).await?;
        report.missions_completed = completed;
        report.missions_expired = expired;
        for carrier in load_carriers(&self.pool, ws, now, FANS_PER_PASS).await? {
            if let Some(plan) = choose_mission(&carrier.context, now)
                && offer(
                    &self.pool,
                    ws,
                    carrier.role_id,
                    carrier.fan_id,
                    &plan,
                    now,
                )
                .await?
                    .is_some()
            {
                report.missions_offered += 1;
            }
        }
        Ok(report)
    }
}
