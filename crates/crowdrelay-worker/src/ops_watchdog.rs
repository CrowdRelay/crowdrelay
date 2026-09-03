//! Low-frequency operational health watchdog for CrowdRelay's own control plane.
//!
//! Detection, cooldown and recovery state are first-party and durable. Alert
//! state is tracked in `viryaos_ops_alert_state` and exposed via the ops API
//! and control plane UI — but no longer emitted to the outbox. The previous
//! outbox events were forwarded to Discord and produced alert spam that was
//! not actionable from there. FakAP remains the external health probe for
//! API reachability; this watchdog catches silent failures FakAP cannot see.
//!
//! The watchdog monitors three conditions:
//! - `executor.offline` — the API is up but no executor has heartbeated
//!   recently, so nothing can actually execute. This is a silent failure
//!   that FakAP (external health probe) cannot detect.
//! - `execution.unknown_outcome` — autopilot actions are stuck in the
//!   `unknown` execution state: their provider receipts were lost or
//!   their outcomes cannot be established, and the receipt reconciliation
//!   sweep could not resolve them. Operator action (check the provider,
//!   re-file a receipt) is the only resolution path.
//! - `execution.contradicted_outcome` — the newest terminal receipt for an
//!   action says the opposite of the action's persisted status. This is what
//!   `LegalTransition::Conflict` refused to coerce, and until now the
//!   refusal existed only as a log line. See below.
//!
//! # Contradictions are the one condition nothing else can find
//!
//! `LegalTransition::Conflict` documents that the caller must surface a
//! contradiction "to operator visibility (log + ops watchdog), not silently
//! pick the latest thing". Every call site did the first half — `tracing::warn!`
//! and return — and there was no watchdog condition for the second. A Conflict
//! left no durable trace: the action kept its state, the report row landed in
//! the ledger like any other, and the only record that two sources disagreed
//! was a log line nobody alerts on.
//!
//! That was easy to miss because the branch could not fire. The receipt
//! resolver fed an action status to `ActionState::parse`, which reads the
//! ledger's uppercase vocabulary, so it saw every action as `Running` and no
//! `Conflict` arm was reachable from that path. With
//! `ActionState::from_action_status` in place it is reachable, and a
//! provider-confirmed success contradicted by a later failure now stands
//! unresolved with nothing sweeping it — unlike `unknown`, which
//! reconciliation retries.
//!
//! The condition is derived from rows that already exist rather than from a
//! counter the resolver would have to remember to increment: the latest
//! terminal receipt per action, compared against that action's status. That
//! also catches contradictions produced by any other path, including ones
//! that happened while nobody was watching.
//!
//! All other conditions (outbox stalls, webhook dead, proof stalls,
//! autopilot failure bursts) were removed because they are either internal
//! plumbing noise not actionable from Discord, or duplicated by FakAP's
//! external monitoring.

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

const ALERT_REPEAT_AFTER: time::Duration = time::Duration::hours(6);

/// How long an action may remain in `unknown` state before the watchdog
/// alerts. This measures **unknown_age** (time since the action ledger
/// entered `UNKNOWN` state), NOT dispatch_age. Transient unknowns created
/// by the reconciliation sweep are expected to resolve within this window;
/// an unknown that persists longer indicates the operator needs to check
/// the provider manually.
const UNKNOWN_ALERT_AGE_THRESHOLD: Duration = Duration::from_secs(60 * 60);

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
                        Ok(Ok(transitions)) if transitions > 0 => {
                            tracing::debug!(transitions, "CrowdRelay ops watchdog updated alert states");
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
        let mut transitions = 0usize;

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
                    transitions = transitions.saturating_add(1);
                }
            } else if previous.is_some_and(|state| state.active) {
                mark_recovered(&mut transaction, self.workspace_id, condition.key, now).await?;
                transitions = transitions.saturating_add(1);
            }
        }

        transaction.commit().await?;
        Ok(transitions)
    }
}

#[derive(Debug, FromRow)]
struct OpsSnapshot {
    executor_registered: i64,
    executor_active: i64,
    /// Total count of actions in `unknown` status (for details).
    unknown_actions: i64,
    /// Count of `unknown` actions whose `unknown_age` exceeds the alert
    /// threshold — i.e., actions that have been unresolved long enough
    /// to warrant operator attention. Transient unknowns (during active
    /// reconciliation) do not trigger the alert.
    stale_unknown_actions: i64,
    /// Actions whose newest terminal receipt contradicts their persisted
    /// status — the standing `LegalTransition::Conflict` population. No age
    /// threshold: nothing sweeps these, so a contradiction is as unresolved
    /// a minute after it appears as it is a day later.
    contradicted_actions: i64,
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
            count(*) FILTER (WHERE expires_at>now())::bigint AS executor_active,
            (SELECT count(*) FROM viryaos_autopilot_actions a
             WHERE a.workspace_id=$1 AND a.status='unknown')::bigint AS unknown_actions,
            -- stale_unknown_actions: unknown actions whose unknown_age
            -- (from the action ledger's state_entered_at) exceeds the
            -- alert threshold. This avoids alerting on transient unknowns
            -- that the reconciliation sweep is actively resolving.
            (SELECT count(*) FROM viryaos_autopilot_actions a
             JOIN viryaos_action_ledger al ON al.action_id = a.id
             WHERE a.workspace_id=$1 AND a.status='unknown'
               AND al.state='UNKNOWN'
               AND al.state_entered_at < now() - make_interval(secs => $2::double precision)
            )::bigint AS stale_unknown_actions,
            -- contradicted_actions: the standing Conflict population. The
            -- *newest* terminal receipt is the comparison, not any receipt —
            -- an older failure followed by a success is an ordinary history,
            -- not a contradiction. Both directions count: a failure receipt
            -- refused against a provider-confirmed success, and a success
            -- receipt refused against a persisted failure.
            (SELECT count(*) FROM viryaos_autopilot_actions a
             JOIN LATERAL (
                 SELECT r.status
                 FROM viryaos_autopilot_execution_reports r
                 WHERE r.workspace_id = a.workspace_id AND r.action_id = a.id
                   AND r.status IN ('succeeded', 'failed')
                 ORDER BY r.occurred_at DESC, r.id DESC
                 LIMIT 1
             ) latest ON true
             WHERE a.workspace_id=$1
               AND ((a.status = 'succeeded' AND latest.status = 'failed')
                 OR (a.status = 'failed' AND latest.status = 'succeeded'))
            )::bigint AS contradicted_actions
        FROM viryaos_executor_instances WHERE workspace_id=$1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(UNKNOWN_ALERT_AGE_THRESHOLD.as_secs() as i64)
    .fetch_one(&mut **transaction)
    .await
}

fn conditions(snapshot: &OpsSnapshot) -> Vec<Condition> {
    vec![
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
            key: "execution.unknown_outcome",
            severity: "warning",
            summary: "Autopilot actions stuck in unknown execution outcome",
            active: snapshot.stale_unknown_actions > 0,
            details: json!({
                "unknown_actions": snapshot.unknown_actions,
                "stale_unknown_actions": snapshot.stale_unknown_actions,
            }),
        },
        Condition {
            key: "execution.contradicted_outcome",
            // Warning, not critical: the state machine already refused to
            // act on the contradiction, so nothing is being corrupted. What
            // is missing is a person deciding which source was right.
            severity: "warning",
            summary: "Autopilot action status contradicted by its newest executor receipt",
            active: snapshot.contradicted_actions > 0,
            details: json!({
                "contradicted_actions": snapshot.contradicted_actions,
            }),
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

#[cfg(test)]
mod tests {
    use super::{OpsSnapshot, conditions};

    fn healthy() -> OpsSnapshot {
        OpsSnapshot {
            executor_registered: 1,
            executor_active: 1,
            unknown_actions: 0,
            stale_unknown_actions: 0,
            contradicted_actions: 0,
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

    #[test]
    fn transient_unknown_does_not_alert() {
        // Unknown actions that are within the alert age threshold
        // (stale_unknown_actions = 0) should NOT trigger the alert —
        // the reconciliation sweep may still resolve them.
        let mut snapshot = healthy();
        snapshot.unknown_actions = 2;
        snapshot.stale_unknown_actions = 0;
        let active = conditions(&snapshot)
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(!active.contains(&"execution.unknown_outcome"));
    }

    #[test]
    fn stale_unknown_action_outcomes_are_detected() {
        // Unknown actions whose unknown_age exceeds the threshold
        // should trigger the alert.
        let mut snapshot = healthy();
        snapshot.unknown_actions = 2;
        snapshot.stale_unknown_actions = 1;
        let active = conditions(&snapshot)
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(active.contains(&"execution.unknown_outcome"));
        assert!(!active.contains(&"executor.offline"));
    }

    #[test]
    fn contradicted_outcomes_are_detected_with_no_age_grace() {
        // A contradiction has no sweep behind it — unlike `unknown`, nothing
        // will resolve it on its own — so a single one alerts immediately
        // rather than waiting out a staleness threshold.
        let mut snapshot = healthy();
        snapshot.contradicted_actions = 1;
        let active = conditions(&snapshot)
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect::<Vec<_>>();
        assert!(active.contains(&"execution.contradicted_outcome"));
        assert!(!active.contains(&"execution.unknown_outcome"));
    }

    #[test]
    fn a_resolved_action_with_older_receipts_is_not_a_contradiction() {
        // The snapshot query compares only the *newest* terminal receipt, so
        // an ordinary failure-then-success history contributes nothing here.
        // Guard the condition side of that: zero means silent.
        let mut snapshot = healthy();
        snapshot.unknown_actions = 3;
        snapshot.contradicted_actions = 0;
        assert!(conditions(&snapshot).iter().all(|condition| condition.key
            != "execution.contradicted_outcome"
            || !condition.active));
    }
}
