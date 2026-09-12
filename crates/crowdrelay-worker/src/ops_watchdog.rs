//! Low-frequency operational health watchdog for CrowdRelay's own control plane.
//!
//! Detection, cooldown and recovery state are first-party and durable. Alert
//! state is tracked in `viryaos_ops_alert_state` and exposed via the ops API
//! and control plane UI — but no longer emitted to the outbox. The previous
//! outbox events were forwarded to Discord and produced alert spam that was
//! not actionable from there. FakAP remains the external health probe for
//! API reachability; this watchdog catches silent failures FakAP cannot see.
//!
//! The watchdog monitors six conditions:
//! - `learning.outcomes_unverified` — every agent outcome in the last day was
//!   refused because no grounding check ran, and none were accepted. Refusing
//!   an unchecked outcome is correct and fail-closed; the cost is that a broken
//!   verifier looks exactly like a quiet system. Production stood in this state
//!   for nine days while every verifier returned HTTP 429 against an exhausted
//!   free-tier quota: 94 outcomes refused, no post ever drafted, and — because
//!   nothing was ever published — the measurement path correctly declined to
//!   resolve any evidence at all. Zero resolved outcomes, zero learning, and
//!   not one alarm. Nothing downstream of a refusal works, so this is critical
//!   rather than a warning.
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
//! - `growth.feed_failing` — a growth feed's last sync attempt failed while
//!   others still work. A credential to go and repair.
//! - `growth.all_feeds_failing` — every feed the tenant has is failing, so
//!   `GrowthStrategy::discovery_channels_are_silent` holds and the brain will
//!   not plan discovery through any of them. The acquisition side of the North
//!   Star is shut until a person restores a credential, and nothing recovers it
//!   automatically. Production stood in exactly this state for weeks — every
//!   Reddit connection failing on an invalid credential — and none of the three
//!   conditions above could see it, because all three watch the executor rather
//!   than the channels the brain grows through.
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
    /// Platforms with at least one connection whose last sync attempt failed.
    ///
    /// Counted by platform, not by connection: twenty-nine subreddits behind
    /// one invalid Reddit credential are one thing to go and fix, and reporting
    /// twenty-nine would make the number look like a catastrophe and read like
    /// noise.
    failing_platforms: i64,
    /// Platforms with at least one connection that has synced and is not
    /// currently failing. Zero alongside a non-zero `failing_platforms` is the
    /// state in which the brain can no longer plan any discovery at all.
    working_platforms: i64,
    /// Cities that fans have requested but that have no coordinates and have
    /// exhausted their geocode attempts. Every fan sitting in one of these
    /// cities is unreachable by the nearby-show loop — the geocoder gave up,
    /// and nothing else supplies coordinates. A human must either fix the
    /// name or enter the coordinates by hand.
    ///
    /// Only cities with `geocode_attempts >= MAX_GEOCODE_ATTEMPTS` count: a
    /// city still being retried is in progress, not stuck.
    stuck_ungeocoded_cities: i64,
    /// Active fans waiting in those cities. The count of cities cannot say
    /// whether one city is holding thirty people or thirty are holding one
    /// each, and those need different responses.
    fans_awaiting_geocoding: i64,
    /// Agent outcomes refused in the last day because no grounding check ran.
    ///
    /// `VerificationStatus::NotVerified` is fail-closed on purpose: an outcome
    /// nobody checked must not become an action. The cost of that correctness
    /// is that a broken verifier is indistinguishable from a quiet system —
    /// the brain proposes, every proposal is refused, and the funnel reports
    /// nothing wrong because nothing errored.
    ///
    /// It happened. Every verifier returned HTTP 429 against an exhausted
    /// free-tier daily quota, 94 outcomes were refused over nine days, no post
    /// was ever drafted, and with no published artifact the measurement path
    /// correctly declined to resolve any of it. Zero resolved outcomes, zero
    /// learning, and not one alarm — the whole autonomous loop was shut by a
    /// quota and the only trace was a rejection reason in a table nobody reads.
    outcomes_rejected_unverified: i64,
    /// Agent outcomes accepted in the same window. Zero accepted alongside a
    /// non-zero refusal count is a stopped loop; a few refusals beside healthy
    /// traffic is a verifier doing its job.
    outcomes_accepted: i64,
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
            )::bigint AS contradicted_actions,
            -- Feed health, by platform. `health` is a generated column, so this
            -- is the same answer `/ops/connections` gives and cannot drift from
            -- it. Both halves are needed: failing feeds alone is a credential to
            -- fix, failing feeds with nothing working is the brain having no
            -- channel left to reason through.
            (SELECT count(DISTINCT platform) FROM fanbase_connections c
             WHERE c.workspace_id=$1 AND c.health='failing')::bigint
                AS failing_platforms,
            (SELECT count(DISTINCT platform) FROM fanbase_connections c
             WHERE c.workspace_id=$1 AND c.health='working')::bigint
                AS working_platforms,
            -- Cities that fans requested but the geocoder gave up on, and
            -- that an active fan is actually waiting in. These are
            -- unreachable by the nearby-show loop until a human fixes the
            -- name or enters coordinates by hand. The attempt threshold
            -- matches `city_geocoding::MAX_GEOCODE_ATTEMPTS` (5).
            --
            -- The fan predicate is what makes the finding mean what it says.
            -- Without it this counted rows, not people: production raised the
            -- warning for a day over "Example City, Example Region" and
            -- "Tes5, Test" -- two test entries the geocoder correctly refused
            -- five times because neither exists -- while the summary claimed
            -- fans there were unreachable. No fan had ever selected either.
            --
            -- A stuck city nobody is waiting in is a data-quality note, not
            -- an operator alarm. It starts mattering the moment a fan picks
            -- it, and that is exactly when this now fires.
            (SELECT count(*) FROM cities ct
             WHERE ct.latitude IS NULL
               AND ct.moderation_status IN ('pending', 'approved')
               AND ct.geocode_attempts >= 5
               AND EXISTS (
                     SELECT 1 FROM fan_location_preferences p
                     JOIN fans f ON f.id = p.fan_id
                     WHERE p.city_id = ct.id
                       AND p.workspace_id = $1
                       AND f.status = 'active'
                   )
            )::bigint AS stuck_ungeocoded_cities,
            -- How many people are behind that count. One city with thirty
            -- fans waiting and thirty cities with one each are different
            -- problems, and the count of cities alone cannot tell them apart.
            (SELECT count(DISTINCT p.fan_id) FROM fan_location_preferences p
             JOIN fans f ON f.id = p.fan_id
             JOIN cities ct ON ct.id = p.city_id
             WHERE p.workspace_id = $1
               AND f.status = 'active'
               AND ct.latitude IS NULL
               AND ct.moderation_status IN ('pending', 'approved')
               AND ct.geocode_attempts >= 5
            )::bigint AS fans_awaiting_geocoding,
            -- Agent outcomes refused in the last day for want of a grounding
            -- check, and the accepted count beside it. A day, not all time:
            -- this asks whether the loop is running now, and a rejection from
            -- last month is history rather than an alarm.
            (SELECT count(*) FROM agent_outcomes o
             WHERE o.workspace_id=$1 AND o.status='rejected'
               AND o.rejection_reason LIKE 'NOT_GROUNDING_CHECKED%'
               AND o.created_at > now() - interval '1 day'
            )::bigint AS outcomes_rejected_unverified,
            (SELECT count(*) FROM agent_outcomes o
             WHERE o.workspace_id=$1 AND o.status='processed'
               AND o.created_at > now() - interval '1 day'
            )::bigint AS outcomes_accepted
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
            // Critical, and deliberately not a warning. Nothing downstream of
            // this works: an outcome that is refused never becomes an action,
            // so no post is drafted, no artifact is published, and the
            // measurement path correctly declines to resolve evidence for a
            // dispatch that never reached anybody. Every posterior stays on
            // its prior. The brain is not degraded, it is disconnected, and no
            // other condition on this list can tell you so.
            //
            // The predicate needs both halves. Refusals alongside healthy
            // traffic are a verifier doing its job on bad output; refusals
            // with nothing accepted is the loop stopped.
            key: "learning.outcomes_unverified",
            severity: "critical",
            summary: "Every agent outcome is being refused for want of a grounding check",
            active: snapshot.outcomes_rejected_unverified > 0 && snapshot.outcomes_accepted == 0,
            details: json!({
                "rejected_unverified": snapshot.outcomes_rejected_unverified,
                "accepted": snapshot.outcomes_accepted,
                "window": "1 day",
                "remedy": "check the agent service's verifier: the reason is in \
                           agent_outcomes.payload->'provenance'->'verification'->>'verifier_error'",
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
        Condition {
            key: "growth.feed_failing",
            // Warning: the tenant still has a channel the brain can work
            // through, and nothing is being lost while this stands.
            severity: "warning",
            summary: "A growth feed's last sync attempt failed",
            active: snapshot.failing_platforms > 0 && snapshot.working_platforms > 0,
            details: json!({
                "failing_platforms": snapshot.failing_platforms,
                "working_platforms": snapshot.working_platforms,
            }),
        },
        Condition {
            // Every feed the tenant has is failing. The brain will not plan
            // discovery through them -- see
            // `GrowthStrategy::discovery_channels_are_silent` -- so the top of
            // the funnel is shut until a person restores a credential. There is
            // no automatic recovery from this and nothing else on this list
            // says it.
            //
            // Critical rather than warning, and separate from
            // `growth.feed_failing` rather than an escalation of it, because
            // the two ask for different things: one is a feed to repair, this
            // is the whole acquisition side of the North Star stopped.
            key: "growth.all_feeds_failing",
            severity: "critical",
            summary: "Every growth feed is failing — the brain cannot discover through any channel",
            active: snapshot.failing_platforms > 0 && snapshot.working_platforms == 0,
            details: json!({
                "failing_platforms": snapshot.failing_platforms,
            }),
        },
        Condition {
            // Cities that fans requested but the geocoder gave up on. Every
            // fan sitting in one is unreachable by the nearby-show loop — the
            // only automatic reason an installed app reopens itself — and
            // nothing recovers this on its own. The geocoding worker may be
            // disabled (missing contact config) or the provider may not
            // recognize the name; either way a human must fix it.
            //
            // Warning, not critical: the nearby-show loop still reaches fans
            // in geocoded cities, and the stuck cities are a growing gap, not
            // a total stop.
            key: "growth.stuck_ungeocoded_cities",
            severity: "warning",
            summary: "Fan-requested cities have exhausted geocoding — fans there are unreachable by nearby-show",
            // Both counts come from the same predicate, so a city only
            // reaches this finding when a fan is behind it. The summary's
            // claim is now load-bearing rather than decorative.
            active: snapshot.stuck_ungeocoded_cities > 0,
            details: json!({
                "stuck_ungeocoded_cities": snapshot.stuck_ungeocoded_cities,
                "fans_awaiting_geocoding": snapshot.fans_awaiting_geocoding,
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
            failing_platforms: 0,
            working_platforms: 2,
            stuck_ungeocoded_cities: 0,
            fans_awaiting_geocoding: 0,
            outcomes_rejected_unverified: 0,
            outcomes_accepted: 4,
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

    /// The keys `conditions` reports as active for a snapshot.
    fn active_keys(snapshot: &OpsSnapshot) -> Vec<&'static str> {
        conditions(snapshot)
            .into_iter()
            .filter(|condition| condition.active)
            .map(|condition| condition.key)
            .collect()
    }

    #[test]
    fn one_failing_feed_beside_a_working_one_is_a_warning() {
        let mut snapshot = healthy();
        snapshot.failing_platforms = 1;
        snapshot.working_platforms = 2;
        let active = active_keys(&snapshot);
        assert!(active.contains(&"growth.feed_failing"));
        assert!(!active.contains(&"growth.all_feeds_failing"));
    }

    #[test]
    fn every_feed_failing_is_critical_and_not_also_a_warning() {
        // Production's standing state: every Reddit connection failing on an
        // invalid credential, for weeks, with nothing on the watchdog saying
        // so. The brain will not plan discovery through silent feeds, so this
        // is the whole acquisition side of the North Star stopped until a
        // person restores a credential -- and nothing recovers it on its own.
        //
        // The two conditions are mutually exclusive on purpose: raising both
        // would put the same fact on the operator's list twice, and an
        // exception list that repeats itself stops being read.
        let mut snapshot = healthy();
        snapshot.failing_platforms = 1;
        snapshot.working_platforms = 0;
        let active = active_keys(&snapshot);
        assert!(active.contains(&"growth.all_feeds_failing"));
        assert!(!active.contains(&"growth.feed_failing"));
    }

    #[test]
    fn a_tenant_with_no_feeds_at_all_raises_nothing() {
        // Nothing connected is not a failure. A tenant who has not connected a
        // feed yet has none that could be broken, and alerting on that would
        // fire on every new workspace from its first cycle.
        let mut snapshot = healthy();
        snapshot.failing_platforms = 0;
        snapshot.working_platforms = 0;
        let active = active_keys(&snapshot);
        assert!(!active.contains(&"growth.feed_failing"));
        assert!(!active.contains(&"growth.all_feeds_failing"));
    }

    #[test]
    fn stuck_ungeocoded_cities_are_detected() {
        // Cities that fans requested but the geocoder gave up on. Every fan in
        // one is unreachable by the nearby-show loop until a human fixes the
        // name or enters coordinates by hand.
        let mut snapshot = healthy();
        snapshot.stuck_ungeocoded_cities = 3;
        snapshot.fans_awaiting_geocoding = 7;
        let active = active_keys(&snapshot);
        assert!(active.contains(&"growth.stuck_ungeocoded_cities"));
    }

    /// The count now comes from a query that requires a waiting fan, so a
    /// stuck city nobody selected never reaches this snapshot at all.
    ///
    /// Production raised this warning for a day over two test rows --
    /// "Example City, Example Region" and "Tes5, Test" -- which the geocoder
    /// correctly refused five times because neither place exists, while the
    /// summary told the operator that fans there were unreachable. Nobody had
    /// ever selected either. The finding was true about rows and false about
    /// people, and only the second reading is worth waking anyone for.
    #[test]
    fn a_stuck_city_with_nobody_waiting_is_not_an_alarm() {
        let snapshot = healthy();
        assert_eq!(snapshot.stuck_ungeocoded_cities, 0);
        assert_eq!(snapshot.fans_awaiting_geocoding, 0);
        assert!(!active_keys(&snapshot).contains(&"growth.stuck_ungeocoded_cities"));
    }

    #[test]
    fn no_stuck_cities_raises_nothing() {
        // A tenant with no stuck cities (either no requests, or all resolved,
        // or still being retried) should not trigger the alert.
        let snapshot = healthy();
        let active = active_keys(&snapshot);
        assert!(!active.contains(&"growth.stuck_ungeocoded_cities"));
    }
}
