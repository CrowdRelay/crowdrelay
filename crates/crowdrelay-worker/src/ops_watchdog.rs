//! Low-frequency operational health watchdog for CrowdRelay's own control plane.
//!
//! Detection, cooldown and recovery state are first-party and durable. Alert
//! state is tracked in `viryaos_ops_alert_state` and exposed via the ops API
//! and control plane UI — but no longer emitted to the outbox. The previous
//! outbox events were forwarded to Discord and produced alert spam that was
//! not actionable from there. FakAP remains the external health probe for
//! API reachability; this watchdog catches silent failures FakAP cannot see.
//!
//! The watchdog monitors ten conditions:
//! - `publishing.orphaned_draft` — a publishing action succeeded and no
//!   executor produced a post for it. The three executors claim
//!   `agent.content.request` by the agent task's `template_id`, and the social
//!   one also filters on platform, while the agents service's schema permits
//!   platforms that filter excludes. Such a draft is claimed by nobody and sits
//!   succeeded-with-no-artifact for good. A static gate cannot see this: the
//!   two halves live in different repositories and CI has no credential to
//!   check out the other one, so the rows say it instead.
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
//! - `delivery.growth_event_refused` — a growth-carrying outbox event was
//!   permanently refused by its consumer. A 4xx is `http_permanent_status`, so
//!   the outbox correctly stops retrying and the delivery is `cancelled` rather
//!   than `dead` — which means `ops/attention`, reporting dead deliveries and a
//!   bare cancelled count, showed four refused press pitches as four increments
//!   in a number that also held 39 stale refusals from August.
//! - `growth.unscoreable_live_opportunities` — the brain scored live
//!   opportunities and denied every one. Read off its own denied decisions, not
//!   off a guess at why: the first version counted rows missing strategic value
//!   and logistics, which was the true reason that morning, and when an import
//!   filled strategic value the alarm went quiet while all 430 opportunities
//!   stayed held on the confidence gate instead. A proxy for a hold stops tracking
//!   the hold. The gate itself is worth knowing: a live opportunity's confidence
//!   is `7500 + (score - minimum_score) * 100`, and `disposition()` denies below
//!   `minimum_confidence`, so 8000 against a score floor of 65 makes the real
//!   floor 70.
//! - `publishing.duplicate_community_draft` — two unpublished drafts target the
//!   same community. Reddit is case-insensitive, so duplicate `discovery_places`
//!   rows drafted one post each for what is one place. Publishing both is posting
//!   twice under the band's name, and the queue is the last point where that is
//!   still cheap to prevent.
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
    /// Actionable agent outcomes accepted in the same window.
    ///
    /// Restricted to the `require_approval` kinds, because those are the only
    /// ones the grounding gate can refuse. Counting observations here compared
    /// two different populations and made the condition unfirable.
    outcomes_accepted: i64,
    /// Publishing actions that succeeded but produced no post artifact, long
    /// enough ago that an executor would have claimed them.
    ///
    /// The three executors claim `agent.content.request` by the agent task's
    /// `template_id`, and the social one also filters on
    /// `platform IN ('instagram','facebook','x')` — while the agents service's
    /// schema lets a `social-post` draft carry `telegram` or `discord`. Such a
    /// draft is claimed by nobody and sits succeeded-with-no-artifact for good.
    ///
    /// A static gate cannot catch this: the two halves live in different
    /// repositories and CI has no credential to check out the other one. The
    /// rows can say it instead, and they catch a mismatch this precise pair
    /// does not cover as well.
    orphaned_publishing_actions: i64,
    /// The same population with no window, reported for context only.
    ///
    /// An orphaned draft is never published later, so the windowed count is
    /// what an operator can still act on and this one is history. It travels in
    /// the details so bounding the alarm hides nothing.
    orphaned_publishing_actions_all_time: i64,
    /// Growth events whose delivery was permanently refused in the last 7 days.
    ///
    /// The event list is deliberately narrow. A refused `ops.status_changed` is
    /// a stale consumer contract and costs nothing; a refused
    /// `agent.content_requested` is a press pitch the brain drafted, addressed
    /// to a named journalist, that no longer has any route to them.
    refused_growth_deliveries: i64,
    /// Live opportunities whose score ceiling is below the score floor.
    ///
    /// Not "none scored well" — *cannot* score well. With no strategic value and
    /// nothing to cost a trip from, 40 of the 100 points are unreachable and the
    /// best possible total is 60 against a minimum of 65.
    unscoreable_live_opportunities: i64,
    /// Unpublished community drafts beyond the first for their community.
    ///
    /// The count of redundant drafts, not of affected communities: two queued
    /// drafts for one subreddit is one double-post to prevent.
    duplicate_community_drafts: i64,
    /// Phases that failed in every one of the last `RELENTLESS_CYCLE_WINDOW`
    /// consecutive cycles, comma-separated, or `None`.
    ///
    /// `outcome = 'degraded'` alone cannot answer the operator's question.
    /// Production ran 296 cycles in a day with 40 degraded, and 13% is either
    /// phase isolation absorbing transient errors — the design working — or one
    /// phase broken every cycle. Those call for opposite responses.
    ///
    /// Consecutiveness is the discriminator, deliberately rather than a share
    /// threshold: what fraction counts as broken is arbitrary, while a phase
    /// that has failed every cycle for an hour is not transient by any reading.
    relentless_degraded_phases: Option<String>,
    /// Which data-quality guards refused an agent outcome in the last day, with
    /// counts, as `GUARD=n` pairs.
    ///
    /// `rejection_reason` was written on every refused row and read by nothing
    /// except the grounding-check prefix above. The operator saw a count and
    /// could not tell the four apart, and they call for opposite responses:
    /// INSUFFICIENT_EVIDENCE is a dead connector, MISSING_TARGET_IDENTITY is one
    /// bad model answer, NOT_GROUNDING_CHECKED is the verifier, and
    /// OFF_PLATFORM_PUSH_TARGET is a model proposing to send the fanbase
    /// somewhere nobody approved.
    ///
    /// Travels in details rather than gating a condition: a guard firing is the
    /// guard working, and four refusals beside twenty-one accepted outcomes is
    /// not a fault. What was missing was the ability to read it at all.
    outcome_rejection_reasons: Option<String>,
    /// Signal pushes refused in the last day because the deep link left the app.
    ///
    /// Its own field, and its own condition, because this one is not a data
    /// quality nit. `target_path` is an in-app route; a model writing an
    /// absolute URL or a scheme there proposes to send the whole fanbase to a
    /// destination nobody approved, and the approval click shows the copy rather
    /// than the link. The guard catches it every time — that is exactly why a
    /// model doing it repeatedly must be visible rather than silently absorbed.
    off_platform_push_attempts: i64,
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

/// How many consecutive cycles a phase must fail before it is not transient.
///
/// Twelve, which is about an hour at the default five-minute cycle. Long enough
/// that a provider blip or a lock contention has resolved; short enough that a
/// genuinely broken phase is reported within the hour rather than the day.
const RELENTLESS_CYCLE_WINDOW: i64 = 12;

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
            -- Accepted *actionable* outcomes only.
            --
            -- The grounding gate applies to `require_approval` kinds alone, so
            -- the refusals counted above are all actionable. Counting every
            -- accepted kind against them compares two different populations:
            -- `recommend_only` insights and segments pass the gate untouched
            -- and keep arriving, so the accepted total is never zero and the
            -- condition could never fire — in exactly the state it exists to
            -- report. Production proved that on the first cycle after deploy:
            -- refusals present, alarm silent.
            (SELECT count(*) FROM agent_outcomes o
             WHERE o.workspace_id=$1 AND o.status='processed'
               AND o.kind IN ('press_pitch','social_post','signal_push','outreach_targets')
               AND o.created_at > now() - interval '1 day'
            )::bigint AS outcomes_accepted,
            -- Succeeded publishing actions with no artifact in any of the four
            -- post tables. The 30-minute floor is the executors' poll window:
            -- below it an action is in flight, not orphaned.
            --
            -- Bounded at 7 days, matching `refused_growth_deliveries`, because
            -- an orphaned draft is unrecoverable: nothing publishes it later
            -- and there is no acknowledgement mechanism, so an unbounded count
            -- made this condition permanently active once a single orphan
            -- existed. An alarm that can never clear is one an operator learns
            -- to ignore. The all-time count still travels in the details, so
            -- the history is reported, just not alarmed on forever.
            (SELECT count(*) FROM viryaos_autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='succeeded'
               AND a.action_kind IN ('agent.content.request',
                                     'community.engage.request')
               AND a.finished_at < now() - interval '30 minutes'
               AND a.finished_at > now() - interval '7 days'
               AND NOT EXISTS (SELECT 1 FROM community_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM telegram_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM discord_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM social_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
            )::bigint AS orphaned_publishing_actions,
            (SELECT count(*) FROM viryaos_autopilot_actions a
             WHERE a.workspace_id=$1
               AND a.status='succeeded'
               AND a.action_kind IN ('agent.content.request',
                                     'community.engage.request')
               AND a.finished_at < now() - interval '30 minutes'
               AND NOT EXISTS (SELECT 1 FROM community_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM telegram_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM discord_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
               AND NOT EXISTS (SELECT 1 FROM social_posts p
                               WHERE p.workspace_id=$1 AND p.action_id=a.id)
            )::bigint AS orphaned_publishing_actions_all_time,
            -- Growth-carrying events whose delivery was permanently refused.
            --
            -- Only the event types that carry work outward: a pitch, a community
            -- engagement, a push proposal. A refused `ops.status_changed` is a
            -- stale consumer contract and costs nothing; a refused
            -- `agent.content_requested` is the press pitch the brain drafted,
            -- addressed to a journalist, never sent.
            --
            -- Cancelled, not dead, which is why nothing reported this. A 4xx is
            -- `http_permanent_status` and the outbox correctly stops retrying —
            -- so the delivery leaves the pending set, never becomes dead, and
            -- `ops/attention`'s `dead_deliveries` stays empty while the event is
            -- just as undelivered. The only trace was the `cancelled` count,
            -- a bare number with no breakdown.
            (SELECT count(*) FROM webhook_deliveries d
             JOIN outbox_events e ON e.id = d.outbox_event_id
             WHERE d.workspace_id=$1
               AND d.status='cancelled'
               AND d.cancelled_at > now() - interval '7 days'
               AND e.event_type IN ('crowdrelay.agent.content_requested',
                                    'crowdrelay.community.engagement_requested')
            )::bigint AS refused_growth_deliveries,
            -- Live opportunities the brain scored and then denied.
            --
            -- Read off the decisions the brain actually recorded rather than by
            -- recomputing its arithmetic here. The first version of this condition
            -- counted rows with `strategic_value_basis_points = 0` and no logistics,
            -- which was the true reason on the day it was written — and the moment
            -- an import filled strategic value, the alarm went quiet while all 430
            -- opportunities stayed held for a different reason. A proxy for a hold
            -- stops tracking the hold; the decision row does not.
            --
            -- `apply_live_opportunity` with `deny` is a precise state: the
            -- opportunity cleared `minimum_score`, so it was worth scoring, and was
            -- refused anyway. In production that is the confidence gate —
            -- `disposition()` denies below `minimum_confidence`, and a live
            -- opportunity's confidence is `7500 + (score - minimum_score) * 100`,
            -- so a `minimum_confidence` of 8000 makes the effective floor five
            -- points above the score floor an operator set. 65 in the policy is 70
            -- in practice, and nothing said so.
            (SELECT count(*) FROM viryaos_autopilot_decisions d
             WHERE d.workspace_id=$1
               AND d.decision_kind='apply_live_opportunity'
               AND d.disposition='deny'
               AND d.evaluated_at > now() - interval '1 day'
            )::bigint AS unscoreable_live_opportunities,
            -- Unpublished drafts that target the same community as another.
            --
            -- Reddit treats subreddit names case-insensitively, so `r/MetalMemes`
            -- and `r/metalmemes` are one place. Duplicate `discovery_places` rows
            -- drafted one post each, and migration 0259 collapsed the places but
            -- deliberately left the drafts alone: they carry different text the
            -- band wrote, and deleting either is not a migration's call.
            --
            -- Two posts to one community months apart are ordinary. Two sitting in
            -- the queue at once are a double-post waiting for whoever publishes
            -- them, which is the spam the North Star rules out — so the condition
            -- is scoped to drafts that are *both* still unpublished.
            (SELECT COALESCE(sum(drafts - 1), 0) FROM (
                SELECT count(*) AS drafts
                FROM community_posts
                WHERE workspace_id=$1
                  AND status='awaiting_manual_post'
                GROUP BY lower(subreddit)
                HAVING count(*) > 1
             ) AS duplicated)::bigint AS duplicate_community_drafts,
            -- Phases that failed in EVERY one of the last N closed cycles.
            --
            -- `HAVING count(*) = (SELECT count(*) FROM recent)` is the
            -- intersection of the recent phase arrays: a phase appearing once per
            -- cycle in all of them failed all of them. The second condition
            -- requires a full window, so a worker that has only just started does
            -- not report its first two cycles as a relentless failure.
            --
            -- Only cycles that recorded the column. Rows from before migration
            -- 0261 carry NULL, which means "did not look" rather than "nothing
            -- failed", and counting them as clean would suppress the alarm.
            (SELECT string_agg(relentless.phase, ',' ORDER BY relentless.phase)
             FROM (
                WITH recent AS (
                    SELECT degraded_phases
                    FROM viryaos_autopilot_cycle_runs
                    WHERE workspace_id=$1
                      AND finished_at IS NOT NULL
                      AND degraded_phases IS NOT NULL
                    ORDER BY started_at DESC
                    LIMIT $3
                )
                SELECT failures.phase
                FROM (SELECT unnest(degraded_phases) AS phase FROM recent) AS failures
                GROUP BY failures.phase
                HAVING count(*) = (SELECT count(*) FROM recent)
                   AND (SELECT count(*) FROM recent) >= $3
             ) AS relentless
            ) AS relentless_degraded_phases,
            -- Guards that refused an outcome in the last day, with counts.
            --
            -- The reason is free text ending in the offending value, so the
            -- prefix before the first colon is the guard identity. Splitting on
            -- it groups "MISSING_TARGET_IDENTITY: display_name is missing" with
            -- every other instance instead of reporting each as unique.
            (SELECT string_agg(guard.name || '=' || guard.hits, ',' ORDER BY guard.name)
             FROM (
                SELECT split_part(o.rejection_reason, ':', 1) AS name,
                       count(*) AS hits
                FROM agent_outcomes o
                WHERE o.workspace_id=$1
                  AND o.status='rejected'
                  AND o.rejection_reason IS NOT NULL
                  AND o.created_at > now() - interval '1 day'
                GROUP BY split_part(o.rejection_reason, ':', 1)
             ) AS guard
            ) AS outcome_rejection_reasons,
            (SELECT count(*) FROM agent_outcomes o
             WHERE o.workspace_id=$1 AND o.status='rejected'
               AND o.rejection_reason LIKE 'OFF_PLATFORM_PUSH_TARGET%'
               AND o.created_at > now() - interval '1 day'
            )::bigint AS off_platform_push_attempts
        FROM viryaos_executor_instances WHERE workspace_id=$1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(UNKNOWN_ALERT_AGE_THRESHOLD.as_secs() as i64)
    .bind(RELENTLESS_CYCLE_WINDOW)
    .fetch_one(&mut **transaction)
    .await
}

include!("ops_watchdog/conditions.rs");

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

include!("ops_watchdog/tests.rs");
