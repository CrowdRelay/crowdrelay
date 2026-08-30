//! Low-frequency operational health watchdog for CrowdRelay's own control plane.
//!
//! Detection, cooldown and recovery state are first-party and durable. External
//! tools only deliver the provider-neutral status event emitted by this worker.
//!
//! The watchdog monitors a single condition: `executor.offline` — the API is
//! up but no executor has heartbeated recently, so nothing can actually
//! execute. This is a silent failure that FakAP (external health probe) cannot
//! detect. All other conditions (outbox stalls, webhook dead, proof stalls,
//! autopilot failure bursts, reconciliation findings) were removed because
//! they are either internal plumbing noise not actionable from Discord, or
//! duplicated by FakAP's external monitoring.

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
    executor_registered: i64,
    executor_active: i64,
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
        SELECT
            count(*)::bigint AS executor_registered,
            count(*) FILTER (WHERE expires_at>now())::bigint AS executor_active
        FROM viryaos_executor_instances WHERE workspace_id=$1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
}

fn conditions(snapshot: &OpsSnapshot) -> Vec<Condition> {
    vec![Condition {
        key: "executor.offline",
        severity: "critical",
        summary: "ViryaOS executor registry has no live executor",
        active: snapshot.executor_registered > 0 && snapshot.executor_active == 0,
        details: json!({
            "registered": snapshot.executor_registered,
            "active": snapshot.executor_active,
        }),
    }]
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
                WHEN $7 THEN EXCLUDED.last_alerted_at
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
    use super::{OpsSnapshot, conditions};

    fn healthy() -> OpsSnapshot {
        OpsSnapshot {
            executor_registered: 1,
            executor_active: 1,
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
    fn executor_offline_is_detected() {
        let mut snapshot = healthy();
        snapshot.executor_active = 0;
        let active = conditions(&snapshot)
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(active.contains(&"executor.offline"));
    }
}
