//! Low-frequency operational health watchdog for CrowdRelay's own control plane.
//!
//! Detection, cooldown and recovery state are first-party and durable. External
//! tools only deliver the provider-neutral status event emitted by this worker.

use std::{collections::HashMap, time::Duration};

use crowdrelay_domain::WorkspaceId;
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

const OUTBOX_STALL_SECONDS: i64 = 5 * 60;
const DELIVERY_STALL_SECONDS: i64 = 5 * 60;
const PROOF_STALL_SECONDS: i64 = 60 * 60;
/// Executor flows run on roughly hourly n8n schedules and post their receipts
/// when they run, so a healthy gap between an emission and its receipt is one
/// executor cycle, not ten seconds. The window has to clear that cycle with
/// margin: at ten minutes it flapped open on every normal hourly batch and
/// recovered again minutes later, which was pure alert noise.
const EXECUTOR_REPORT_STALL_SECONDS: i64 = 2 * 60 * 60;
const AUTOPILOT_FAILURE_THRESHOLD: i64 = 3;
const ALERT_REPEAT_AFTER: time::Duration = time::Duration::hours(6);

#[derive(Debug, Error)]
enum OpsWatchdogError {
    #[error("operational watchdog database operation failed")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Debug)]
pub struct OpsWatchdogWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
}

impl OpsWatchdogWorker {
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
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticks = interval(self.poll_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticks.tick() => {
                    match timeout(self.operation_timeout, self.run_once()).await {
                        Ok(Ok(emitted)) if emitted > 0 => {
                            tracing::warn!(events = emitted, "CrowdRelay ops watchdog emitted status changes");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "CrowdRelay ops watchdog cycle failed"),
                        Err(_) => tracing::warn!("CrowdRelay ops watchdog cycle timed out"),
                    }
                }
            }
        }
    }

    async fn run_once(&self) -> Result<usize, OpsWatchdogError> {
        let now = OffsetDateTime::now_utc();
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{}:viryaos-ops-watchdog", self.workspace_id))
            .execute(&mut *transaction)
            .await?;
        let snapshot = load_snapshot(&mut transaction, self.workspace_id).await?;
        let conditions = conditions(&snapshot);
        let states = load_states(&mut transaction, self.workspace_id).await?;
        let repeat_before = now
            .checked_sub(ALERT_REPEAT_AFTER)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let mut emitted = 0usize;

        for condition in conditions {
            let previous = states.get(condition.key);
            if condition.active {
                let repeat_due = previous.is_none_or(|state| {
                    !state.active
                        || state
                            .last_alerted_at
                            .is_none_or(|alerted| alerted <= repeat_before)
                });
                upsert_active(
                    &mut transaction,
                    self.workspace_id,
                    &condition,
                    now,
                    repeat_due,
                )
                .await?;
                if repeat_due {
                    emit_status_change(
                        &mut transaction,
                        self.workspace_id,
                        &condition,
                        "open",
                        now,
                    )
                    .await?;
                    emitted = emitted.saturating_add(1);
                }
            } else if previous.is_some_and(|state| state.active) {
                mark_recovered(&mut transaction, self.workspace_id, condition.key, now).await?;
                emit_status_change(
                    &mut transaction,
                    self.workspace_id,
                    &condition,
                    "recovered",
                    now,
                )
                .await?;
                emitted = emitted.saturating_add(1);
            }
        }

        transaction.commit().await?;
        Ok(emitted)
    }
}

#[derive(Debug, FromRow)]
struct OpsSnapshot {
    outbox_dead: i64,
    outbox_oldest_pending_seconds: i64,
    delivery_dead: i64,
    delivery_oldest_pending_seconds: i64,
    proof_dead: i64,
    proof_oldest_pending_seconds: i64,
    executor_registered: i64,
    executor_active: i64,
    awaiting_executor_old: i64,
    autopilot_failed_15m: i64,
    reconciliation_critical_open: i64,
}

#[derive(Clone, Debug)]
struct Condition {
    key: &'static str,
    severity: &'static str,
    summary: &'static str,
    active: bool,
    details: Value,
}

#[derive(Debug, FromRow)]
struct AlertState {
    alert_key: String,
    active: bool,
    last_alerted_at: Option<OffsetDateTime>,
}

async fn load_snapshot(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<OpsSnapshot, sqlx::Error> {
    sqlx::query_as::<_, OpsSnapshot>(
        r#"
        WITH outbox AS (
            SELECT
                count(*) FILTER (WHERE status='dead')::bigint AS dead,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status='pending' AND available_at<=now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM outbox_events WHERE workspace_id=$1
        ), deliveries AS (
            SELECT
                count(*) FILTER (WHERE status='dead')::bigint AS dead,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status='pending' AND available_at<=now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM webhook_deliveries WHERE workspace_id=$1
        ), proofs AS (
            SELECT
                count(*) FILTER (WHERE status='dead')::bigint AS dead,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status IN ('queued','failed') AND available_at<=now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM external_proof_batches WHERE workspace_id=$1
        ), executors AS (
            SELECT
                count(*)::bigint AS registered,
                count(*) FILTER (WHERE expires_at>now())::bigint AS active
            FROM viryaos_executor_instances WHERE workspace_id=$1
        ), executor_lag AS (
            SELECT count(*)::bigint AS awaiting_old
            FROM viryaos_autopilot_action_emissions emission
            JOIN viryaos_autopilot_actions action
              ON action.workspace_id=emission.workspace_id AND action.id=emission.action_id
            WHERE emission.workspace_id=$1
              AND action.status='succeeded'
              AND emission.emitted_at<=now() - ($2 * INTERVAL '1 second')
              AND NOT EXISTS (
                  SELECT 1 FROM viryaos_autopilot_execution_reports report
                  WHERE report.workspace_id=emission.workspace_id
                    AND report.action_id=emission.action_id
                    AND report.status IN ('succeeded','failed')
              )
        ), autopilot AS (
            SELECT count(*)::bigint AS failed_15m
            FROM viryaos_autopilot_actions
            WHERE workspace_id=$1 AND status='failed'
              AND finished_at>=now() - INTERVAL '15 minutes'
        ), reconciliation AS (
            SELECT count(*)::bigint AS critical_open
            FROM reconciliation_findings
            WHERE workspace_id=$1 AND severity='critical' AND resolved_at IS NULL
        )
        SELECT
            outbox.dead AS outbox_dead,
            outbox.oldest_pending_seconds AS outbox_oldest_pending_seconds,
            deliveries.dead AS delivery_dead,
            deliveries.oldest_pending_seconds AS delivery_oldest_pending_seconds,
            proofs.dead AS proof_dead,
            proofs.oldest_pending_seconds AS proof_oldest_pending_seconds,
            executors.registered AS executor_registered,
            executors.active AS executor_active,
            executor_lag.awaiting_old AS awaiting_executor_old,
            autopilot.failed_15m AS autopilot_failed_15m,
            reconciliation.critical_open AS reconciliation_critical_open
        FROM outbox CROSS JOIN deliveries CROSS JOIN proofs CROSS JOIN executors
        CROSS JOIN executor_lag CROSS JOIN autopilot CROSS JOIN reconciliation
        "#,
    )
    .bind(workspace_id.into_uuid())
    // The lag window is a real query parameter, not just alert cosmetics:
    // keeping it in sync with EXECUTOR_REPORT_STALL_SECONDS here is what makes
    // the threshold change mean anything.
    .bind(EXECUTOR_REPORT_STALL_SECONDS)
    .fetch_one(&mut **transaction)
    .await
}

fn conditions(snapshot: &OpsSnapshot) -> Vec<Condition> {
    vec![
        Condition {
            key: "outbox.dead",
            severity: "critical",
            summary: "Outbox events exhausted automatic retries",
            active: snapshot.outbox_dead > 0,
            details: json!({"dead": snapshot.outbox_dead}),
        },
        Condition {
            key: "outbox.stalled",
            severity: "warning",
            summary: "Outbox delivery queue is not draining",
            active: snapshot.outbox_oldest_pending_seconds >= OUTBOX_STALL_SECONDS,
            details: json!({"oldest_pending_seconds": snapshot.outbox_oldest_pending_seconds}),
        },
        Condition {
            key: "webhook.dead",
            severity: "critical",
            summary: "Webhook deliveries exhausted automatic retries",
            active: snapshot.delivery_dead > 0,
            details: json!({"dead": snapshot.delivery_dead}),
        },
        Condition {
            key: "webhook.stalled",
            severity: "warning",
            summary: "Webhook delivery queue is not draining",
            active: snapshot.delivery_oldest_pending_seconds >= DELIVERY_STALL_SECONDS,
            details: json!({"oldest_pending_seconds": snapshot.delivery_oldest_pending_seconds}),
        },
        Condition {
            key: "proof.dead_or_stalled",
            severity: "warning",
            summary: "External proof anchoring needs attention",
            active: snapshot.proof_dead > 0
                || snapshot.proof_oldest_pending_seconds >= PROOF_STALL_SECONDS,
            details: json!({
                "dead": snapshot.proof_dead,
                "oldest_pending_seconds": snapshot.proof_oldest_pending_seconds,
            }),
        },
        Condition {
            key: "executor.offline",
            severity: "critical",
            summary: "ViryaOS executor registry has no live executor",
            active: snapshot.executor_registered > 0 && snapshot.executor_active == 0,
            details: json!({
                "registered": snapshot.executor_registered,
                "active": snapshot.executor_active,
            }),
        },
        Condition {
            key: "executor.report_lag",
            severity: "warning",
            summary: "Provider actions are waiting too long for executor receipts",
            active: snapshot.awaiting_executor_old > 0,
            details: json!({
                "awaiting_over_seconds": EXECUTOR_REPORT_STALL_SECONDS,
                "actions": snapshot.awaiting_executor_old,
            }),
        },
        Condition {
            key: "autopilot.failure_burst",
            severity: "critical",
            summary: "ViryaOS Autopilot has a burst of failed actions",
            active: snapshot.autopilot_failed_15m >= AUTOPILOT_FAILURE_THRESHOLD,
            details: json!({
                "failed_15m": snapshot.autopilot_failed_15m,
                "threshold": AUTOPILOT_FAILURE_THRESHOLD,
            }),
        },
        Condition {
            key: "reconciliation.critical",
            severity: "critical",
            summary: "Ecosystem reconciliation has unresolved critical findings",
            active: snapshot.reconciliation_critical_open > 0,
            details: json!({"open_critical": snapshot.reconciliation_critical_open}),
        },
    ]
}

async fn load_states(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<HashMap<String, AlertState>, sqlx::Error> {
    let rows = sqlx::query_as::<_, AlertState>(
        r#"
        SELECT alert_key, active, last_alerted_at
        FROM viryaos_ops_alert_state
        WHERE workspace_id=$1
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(rows
        .into_iter()
        .map(|state| (state.alert_key.clone(), state))
        .collect())
}

async fn upsert_active(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    condition: &Condition,
    now: OffsetDateTime,
    alerted: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_ops_alert_state (
            workspace_id, alert_key, severity, summary, active,
            first_seen_at, last_seen_at, last_alerted_at, details
        ) VALUES ($1,$2,$3,$4,true,$5,$5,CASE WHEN $7 THEN $5 ELSE NULL END,$6)
        ON CONFLICT (workspace_id, alert_key) DO UPDATE
        SET severity=EXCLUDED.severity,
            summary=EXCLUDED.summary,
            active=true,
            first_seen_at=CASE
                WHEN viryaos_ops_alert_state.active THEN viryaos_ops_alert_state.first_seen_at
                ELSE EXCLUDED.first_seen_at
            END,
            last_seen_at=EXCLUDED.last_seen_at,
            last_alerted_at=CASE
                WHEN $7 THEN EXCLUDED.last_seen_at
                ELSE viryaos_ops_alert_state.last_alerted_at
            END,
            recovered_at=NULL,
            details=EXCLUDED.details
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(condition.key)
    .bind(condition.severity)
    .bind(condition.summary)
    .bind(now)
    .bind(&condition.details)
    .bind(alerted)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn mark_recovered(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    key: &str,
    now: OffsetDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE viryaos_ops_alert_state
        SET active=false, last_seen_at=$3, recovered_at=$3, last_alerted_at=$3
        WHERE workspace_id=$1 AND alert_key=$2 AND active
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(key)
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn emit_status_change(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    condition: &Condition,
    state: &'static str,
    now: OffsetDateTime,
) -> Result<(), sqlx::Error> {
    let request_id = format!(
        "ops-watchdog:{}:{}:{}",
        condition.key,
        state,
        Uuid::now_v7()
    );
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id, max_attempts
        ) VALUES (
            $1, 'crowdrelay.ops.status_changed', 1,
            jsonb_build_object(
                'alert_key', $2::text,
                'state', $3::text,
                'severity', $4::text,
                'summary', $5::text,
                'details', $6::jsonb,
                'observed_at', $7::timestamptz,
                'source', 'crowdrelay-worker'
            ),
            $8, 12
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(condition.key)
    .bind(state)
    .bind(condition.severity)
    .bind(condition.summary)
    .bind(&condition.details)
    .bind(now)
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AUTOPILOT_FAILURE_THRESHOLD, DELIVERY_STALL_SECONDS, OUTBOX_STALL_SECONDS, OpsSnapshot,
        conditions,
    };

    fn healthy() -> OpsSnapshot {
        OpsSnapshot {
            outbox_dead: 0,
            outbox_oldest_pending_seconds: 0,
            delivery_dead: 0,
            delivery_oldest_pending_seconds: 0,
            proof_dead: 0,
            proof_oldest_pending_seconds: 0,
            executor_registered: 1,
            executor_active: 1,
            awaiting_executor_old: 0,
            autopilot_failed_15m: 0,
            reconciliation_critical_open: 0,
        }
    }

    #[test]
    fn healthy_runtime_does_not_raise_attention() {
        assert!(
            conditions(&healthy())
                .iter()
                .all(|condition| !condition.active)
        );
    }

    #[test]
    fn queue_and_executor_failures_are_detected_at_the_boundary() {
        let mut snapshot = healthy();
        snapshot.outbox_oldest_pending_seconds = OUTBOX_STALL_SECONDS;
        snapshot.delivery_oldest_pending_seconds = DELIVERY_STALL_SECONDS;
        snapshot.executor_active = 0;
        snapshot.autopilot_failed_15m = AUTOPILOT_FAILURE_THRESHOLD;
        let active = conditions(&snapshot)
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(active.contains(&"outbox.stalled"));
        assert!(active.contains(&"webhook.stalled"));
        assert!(active.contains(&"executor.offline"));
        assert!(active.contains(&"autopilot.failure_burst"));
    }
}
