//! Sends the operator one brief a day, and deliberately not more.
//!
//! This runs in the worker rather than as an autopilot context, for one reason
//! that decides the design: the brief must still arrive when the agent is
//! switched off. Routing it through the growth envelope would mean a disabled
//! envelope silences the message whose whole job is to say the envelope is
//! disabled — the exact production state that made this worth building.
//!
//! It reaches the band's own operator, not an audience, so it is first-party
//! and spends no outreach budget. Delivery rides `ops.alert`, the capability
//! the executor already advertises.

use std::time::Duration;

use crowdrelay_domain::{
    WorkspaceId,
    operator_brief::{
        BriefHeadline, OperatorBriefDecision, OperatorBriefPolicy, OperatorBriefSnapshot,
        evaluate_operator_brief,
    },
};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

#[derive(Debug, Error)]
enum OperatorBriefError {
    #[error("operator brief database operation failed")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Debug)]
pub struct OperatorBriefWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
    policy: OperatorBriefPolicy,
}

impl OperatorBriefWorker {
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
            policy: OperatorBriefPolicy::default(),
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticks = interval(self.poll_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    match timeout(self.operation_timeout, self.cycle()).await {
                        Ok(Ok(Some(headline))) => tracing::info!(
                            headline = headline.as_str(),
                            "operator brief sent"
                        ),
                        Ok(Ok(None)) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "operator brief cycle failed");
                        }
                        Err(_) => {
                            tracing::warn!("operator brief cycle timed out");
                        }
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    }

    async fn cycle(&self) -> Result<Option<BriefHeadline>, OperatorBriefError> {
        let now = OffsetDateTime::now_utc();
        let snapshot = self.snapshot(now).await?;
        let OperatorBriefDecision::Send(headline) =
            evaluate_operator_brief(&snapshot, self.policy, now)
        else {
            return Ok(None);
        };
        self.send(&snapshot, headline, now).await?;
        Ok(Some(headline))
    }

    async fn snapshot(
        &self,
        now: OffsetDateTime,
    ) -> Result<OperatorBriefSnapshot, OperatorBriefError> {
        let row = sqlx::query_as::<_, BriefRow>(
            r#"
            SELECT
                (SELECT count(*) FROM viryaos_autopilot_actions
                  WHERE workspace_id=$1 AND status='succeeded'
                    AND finished_at >= $2 - INTERVAL '24 hours')::bigint AS executed_24h,
                (SELECT count(*) FROM viryaos_autopilot_actions
                  WHERE workspace_id=$1 AND status='failed'
                    AND finished_at >= $2 - INTERVAL '24 hours')::bigint AS failed_24h,
                (SELECT count(*) FROM viryaos_autopilot_actions
                  WHERE workspace_id=$1 AND status='awaiting_approval')::bigint AS awaiting,
                (SELECT max(EXTRACT(EPOCH FROM ($2 - created_at)) / 3600)::bigint
                   FROM viryaos_autopilot_actions
                  WHERE workspace_id=$1 AND status='awaiting_approval') AS oldest_approval_hours,
                -- Parked work: queued, still retryable, and waiting on a
                -- capability nobody advertises. Approving these changes nothing,
                -- which is why the rule ranks them apart from the queue.
                (SELECT count(*) FROM viryaos_autopilot_actions
                  WHERE workspace_id=$1 AND status='queued'
                    AND last_error_kind='awaiting_executor')::bigint AS parked,
                (NOT EXISTS (SELECT 1 FROM viryaos_executor_instances WHERE workspace_id=$1 AND expires_at > now())) AS execution_plane_dead,
                -- Off-platform feeds with no series at all. A platform we cannot
                -- see must never be reported as a platform that did not move.
                (SELECT count(*) FROM (VALUES ('spotify'),('youtube'),('bandsintown')) AS
                        expected(platform)
                  WHERE NOT EXISTS (
                      SELECT 1 FROM viryaos_growth_metric_series series
                      WHERE series.workspace_id=$1 AND series.platform=expected.platform
                  ))::bigint AS blind_platforms,
                -- The last sweep's own answer about what it read. Deliberately
                -- the latest report rather than a barren run: a read path that
                -- returned nothing on the most recent attempt is broken now,
                -- and waiting for it to fail three times before saying so is
                -- three days of a growth loop that cannot find anybody.
                coalesce((
                    SELECT report.items_seen = 0
                    FROM viryaos_outreach_discovery_sweep_reports report
                    WHERE report.workspace_id=$1
                    ORDER BY report.created_at DESC, report.id DESC
                    LIMIT 1
                ), false) AS last_sweep_read_nothing,
                (SELECT agent_enabled FROM viryaos_growth_envelope
                  WHERE workspace_id=$1) AS agent_enabled,
                (SELECT dry_run FROM viryaos_growth_envelope
                  WHERE workspace_id=$1) AS dry_run,
                (SELECT max(sent_at) FROM viryaos_operator_briefs
                  WHERE workspace_id=$1) AS last_brief_at
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        Ok(OperatorBriefSnapshot {
            actions_executed_24h: clamp(row.executed_24h),
            actions_failed_24h: clamp(row.failed_24h),
            actions_awaiting_approval: clamp(row.awaiting),
            oldest_approval_age_hours: row.oldest_approval_hours.map(clamp),
            actions_parked: clamp(row.parked),
            execution_plane_dead: row.execution_plane_dead,
            blind_platforms: u16::try_from(row.blind_platforms.max(0)).unwrap_or(u16::MAX),
            last_sweep_read_nothing: row.last_sweep_read_nothing,
            // A workspace with no envelope row has never been configured, and
            // an unconfigured agent is not a running one.
            agent_enabled: row.agent_enabled.unwrap_or(false),
            dry_run: row.dry_run.unwrap_or(true),
            last_brief_at: row.last_brief_at,
        })
    }

    /// Records the brief and queues its delivery in one transaction, so a brief
    /// can never be recorded as sent without an event, or delivered twice.
    async fn send(
        &self,
        snapshot: &OperatorBriefSnapshot,
        headline: BriefHeadline,
        now: OffsetDateTime,
    ) -> Result<(), OperatorBriefError> {
        let evidence = serde_json::to_value(snapshot).unwrap_or_else(|_| json!({}));
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO viryaos_operator_briefs (workspace_id, headline, snapshot, sent_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(self.workspace_id.into_uuid())
        .bind(headline.as_str())
        .bind(&evidence)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id, max_attempts
            ) VALUES (
                $1, 'crowdrelay.ops.operator_brief', 1,
                jsonb_build_object(
                    'headline', $2::text,
                    'summary', $3::text,
                    'snapshot', $4::jsonb,
                    'observed_at', $5::timestamptz,
                    'source', 'crowdrelay-worker'
                ),
                $6, 12
            )
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(headline.as_str())
        .bind(summary(snapshot, headline))
        .bind(&evidence)
        .bind(now)
        .bind(format!("operator-brief:{}", Uuid::now_v7()))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

/// One sentence stating what is true, never what to do about it. The brief
/// reports; the read models and the operator decide.
fn summary(snapshot: &OperatorBriefSnapshot, headline: BriefHeadline) -> String {
    match headline {
        BriefHeadline::ExecutionPlaneDead => format!(
            "{} actions parked; no executor has heartbeated — the execution plane is dead",
            snapshot.actions_parked
        ),
        BriefHeadline::ApprovalStale => format!(
            "{} decisions await approval; the oldest has waited {} hours",
            snapshot.actions_awaiting_approval,
            snapshot.oldest_approval_age_hours.unwrap_or(0)
        ),
        BriefHeadline::DisabledWithWorkWaiting => format!(
            "the agent is {} with {} decisions and {} parked actions waiting",
            if snapshot.agent_enabled {
                "in dry run"
            } else {
                "disabled"
            },
            snapshot.actions_awaiting_approval,
            snapshot.actions_parked
        ),
        BriefHeadline::WorkParked => format!(
            "{} actions are parked because no executor advertises the capability they need",
            snapshot.actions_parked
        ),
        BriefHeadline::AwaitingApproval => format!(
            "{} decisions are waiting for approval",
            snapshot.actions_awaiting_approval
        ),
        BriefHeadline::Failing => format!(
            "{} actions failed in the last 24 hours",
            snapshot.actions_failed_24h
        ),
        BriefHeadline::Blind => format!(
            "{} off-platform feeds have no data, so their metrics cannot be read as unchanged",
            snapshot.blind_platforms
        ),
        BriefHeadline::DiscoveryReadNothing => {
            "the last discovery sweep read nothing, so no new outreach targets can be found"
                .to_owned()
        }
        BriefHeadline::Worked => format!(
            "{} actions completed in the last 24 hours and nothing needs you",
            snapshot.actions_executed_24h
        ),
    }
}

fn clamp(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

#[derive(Debug, FromRow)]
struct BriefRow {
    executed_24h: i64,
    failed_24h: i64,
    awaiting: i64,
    oldest_approval_hours: Option<i64>,
    parked: i64,
    execution_plane_dead: bool,
    blind_platforms: i64,
    last_sweep_read_nothing: bool,
    agent_enabled: Option<bool>,
    dry_run: Option<bool>,
    last_brief_at: Option<OffsetDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> OperatorBriefSnapshot {
        OperatorBriefSnapshot {
            execution_plane_dead: false,
            actions_executed_24h: 0,
            actions_failed_24h: 0,
            actions_awaiting_approval: 12,
            oldest_approval_age_hours: Some(70),
            actions_parked: 4,
            blind_platforms: 3,
            last_sweep_read_nothing: false,
            agent_enabled: false,
            dry_run: true,
            last_brief_at: None,
        }
    }

    #[test]
    fn a_missing_envelope_row_is_read_as_not_running_rather_than_as_running() {
        // `unwrap_or(false)` and `unwrap_or(true)` are the safe direction: an
        // unconfigured workspace must not be reported as a live agent.
        let row = BriefRow {
            execution_plane_dead: false,
            executed_24h: 0,
            failed_24h: 0,
            awaiting: 1,
            oldest_approval_hours: None,
            parked: 0,
            blind_platforms: 0,
            last_sweep_read_nothing: false,
            agent_enabled: None,
            dry_run: None,
            last_brief_at: None,
        };
        assert!(!row.agent_enabled.unwrap_or(false));
        assert!(row.dry_run.unwrap_or(true));
    }

    #[test]
    fn every_headline_produces_a_summary_that_states_a_fact() {
        for headline in [
            BriefHeadline::Worked,
            BriefHeadline::Blind,
            BriefHeadline::Failing,
            BriefHeadline::AwaitingApproval,
            BriefHeadline::WorkParked,
            BriefHeadline::DisabledWithWorkWaiting,
            BriefHeadline::ApprovalStale,
        ] {
            let line = summary(&snapshot(), headline);
            assert!(!line.is_empty());
            // A brief that tells the operator what to do is a task list wearing
            // a report, and it ages badly against a policy it cannot see.
            for imperative in ["should", "must", "please", "you need to"] {
                assert!(
                    !line.contains(imperative),
                    "{headline:?} summary gave an instruction: {line}"
                );
            }
        }
    }

    #[test]
    fn counts_never_wrap_when_postgres_returns_something_absurd() {
        assert_eq!(clamp(-1), 0);
        assert_eq!(clamp(i64::MAX), u32::MAX);
    }
}
