//! Low-frequency operational health watchdog for CrowdRelay's own control plane.
//!
//! Detection, cooldown and recovery state are first-party and durable. Alert
//! state is tracked in `ops_alert_state` and exposed via the ops API
//! and control plane UI — but no longer emitted to the outbox. The previous
//! outbox events were forwarded to Discord and produced alert spam that was
//! not actionable from there. FakAP remains the external health probe for
//! API reachability; this watchdog catches silent failures FakAP cannot see.
//!
//! The watchdog monitors 23 conditions. The count and this list are
//! gated against `conditions()` by `test_watchdog_conditions_documented_v1.py`:
//! it said "ten" while seven alarms went undocumented, including two criticals,
//! and this repository has a record of concluding a live capability is missing
//! by reading a stale list. Every condition below also has a test, so none is
//! an alarm nobody has seen fire.
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
//!   in a number that also held 39 stale refusals from August. Letters are
//!   counted by the recipient keys their payloads carry, not by a type list —
//!   the list form watched two types while an approved festival application
//!   and the T+7 show report died 422 on 2026-09-15.
//! - `delivery.event_refused` — refused deliveries carrying no named
//!   recipient: stale consumer contracts and unrouted internal events in the
//!   same cancelled-instead-of-dead blind spot. Warning, not critical.
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
//! - `executor.capability_unadvertised` — the registry is live but work is
//!   parking `awaiting_executor` or cancelling `no_executor`: a capability
//!   fell out of the advertised set while demand for it continues.
//!   Production carried exactly this for eleven days when `team.email`'s
//!   attestation aged past its freshness window — the heartbeat's own
//!   fail-closed design dropped the capability, and nothing said so.
//! - `execution.unknown_outcome` — autopilot actions are stuck in the
//!   `unknown` execution state: their provider receipts were lost or
//!   their outcomes cannot be established, and the receipt reconciliation
//!   sweep could not resolve them. Operator action (check the provider,
//!   re-file a receipt) is the only resolution path.
//! - `execution.contradicted_outcome` — the newest terminal receipt for an
//!   action says the opposite of the action's persisted status. This is what
//!   `LegalTransition::Conflict` refused to coerce, and until now the
//!   refusal existed only as a log line. See below.
//! - `execution.approved_actions_failed` — an action a person approved failed
//!   in the last 24 hours. It is the operator's own work thrown away, and on
//!   2026-09-28 38 approved letters failed at dispatch with nothing to say so.
//! - `autopilot.authority_guarded` — the autonomy guardrail demoted a policy to
//!   `require_approval` after two worsened outcomes and the guard has not
//!   lapsed. The demotion's own event is refused by the n8n router, so this is
//!   the operator's notice that every action in that context now waits for them.
//! - `approval.expired_unanswered` — approvals were cancelled because nobody
//!   answered them inside the 72-hour window. Each is a proposal the brain made,
//!   ranked and queued, that the system then discarded. It is the one loss that
//!   scales with the operator being the bottleneck, and nothing else here sees it:
//!   when it happens the executors are live, the feeds sync, the drafts publish
//!   and the brain cycles. The only trace is `last_error_kind` on a cancelled
//!   row, which is also the only thing separating an expiry from a deliberate
//!   rejection. Reported with the next outstanding deadline, not just the past
//!   count — a count says somebody was too slow, a deadline says what to do today.
//! - `brain.phase_failing_every_cycle` — **critical.** One cycle phase has
//!   failed in every cycle across the window. Consecutiveness rather than a
//!   share, because what fraction counts as broken would need a number nobody
//!   has, and a phase failing every cycle for an hour is not transient under any
//!   reading.
//! - `safety.off_platform_push_proposed` — **critical**, and not conditioned on
//!   anything else failing. A model proposed a Signal push whose target was an
//!   absolute URL rather than an in-app route, which would have sent the whole
//!   fanbase somewhere nobody approved. The guard refused it, so nothing was
//!   sent and no fan was harmed — which is precisely why it would otherwise be
//!   invisible. The approval click shows the copy, not the link, so a human
//!   reviewer would not have caught it either.
//! - `learning.posterior_never_updated` — **critical.** Decisions are being made
//!   and the causal posterior has never seen an observation, so every prediction
//!   the brain reports IS its prior. It reports the one failure nothing else can
//!   see: not that something broke, but that something never started. Cycles
//!   succeed, decisions are written and actions are created throughout. Both
//!   halves are required, because a workspace with few decisions and no
//!   observations has not run yet rather than failed to learn.
//! - `publishing.drafts_with_no_publisher` — drafts are queued and the posture
//!   says nothing will publish them, naming the switch that is missing. Both
//!   halves are required: an empty queue with publishing off is a setting, and a
//!   manual channel with nothing waiting is nothing to report.
//! - `publishing.drafts_failed` — drafts failed and their content is not being
//!   retried, reported **with the reasons**. The reason is the whole point:
//!   "Reddit refused this" and "the agents service was unreachable" call for
//!   opposite responses, and `error_message` was written on every failed row and
//!   read by nothing.
//! - `publishing.session_dead` — Reddit work is queued and no session can post
//!   it. The predicate is the agents service's own eligibility rule rather than a
//!   proxy: stored cookies beside a dead credential are a session that still
//!   cannot post. Drafts retry for about an hour, and a six-hour cooldown
//!   outlives that cover, so the window for a person to act is real.
//! - `growth.stuck_ungeocoded_cities` — fan-requested cities have exhausted
//!   geocoding, so fans there are unreachable by the nearby-show loop. Both
//!   counts come from one predicate, so a city only reaches this finding when a
//!   fan is behind it.
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
//! - `approval.sweep_lagging` — an approval ask outlived its deadline and is
//!   still `awaiting_approval`. Two independent sweeps should have cancelled
//!   it: the claim path sweeps on every autopilot cycle, and the retention
//!   worker sweeps globally every hour. A row this stale means neither ran —
//!   the lapsed read calls it `awaiting_sweep`, hidden from the queue on one
//!   side and uncounted as a loss on the other. Warning, not critical: nothing
//!   was corrupted, and the fix is the sweep running again, not a person's.
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

use crate::auto_post_platforms::PublishingPosture;
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
pub enum OpsWatchdogError {
    #[error("operational watchdog database operation failed")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Debug)]
pub struct OpsWatchdogWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
    /// Whether anything will publish a Reddit draft, and which switch is missing.
    ///
    /// Passed in rather than read here: the watchdog reports on state, and a
    /// process-level switch is not state it should be discovering for itself.
    /// One value is read in `main` so the readiness log, the executor's mode and
    /// this condition cannot disagree about what will publish.
    posture: PublishingPosture,
}

impl OpsWatchdogWorker {
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        operation_timeout: Duration,
        posture: PublishingPosture,
    ) -> Self {
        Self {
            pool,
            workspace_id,
            poll_interval,
            operation_timeout,
            posture,
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

    /// One watchdog cycle. Public so tests can drive the real snapshot +
    /// condition evaluation — the alerting surface is the worst place for a
    /// break that only a live database can see.
    pub async fn run_once(&self) -> Result<usize, OpsWatchdogError> {
        let now = OffsetDateTime::now_utc();
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{}:crowdrelay-ops-watchdog", self.workspace_id))
            .execute(&mut *transaction)
            .await?;
        let snapshot = load_snapshot(&mut transaction, self.workspace_id).await?;
        let conditions = conditions(&snapshot, self.posture);
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
    /// to a named journalist, that no longer has any route to them. The payload
    /// keys — `contact_email`, `recipient_email`, `recipients` — are the
    /// predicate, so a new letter type cannot add itself silently the way the
    /// refused festival application did on 2026-09-15.
    refused_growth_deliveries: i64,
    /// Refused deliveries in the same window that carry no named recipient —
    /// stale consumer contracts and unrouted internal events. Warning-class:
    /// the count exists so a non-letter event type dying at the bridge is
    /// visible without alarming at letter severity.
    refused_other_deliveries: i64,
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
    /// Reddit drafts sitting in `awaiting_manual_post`.
    ///
    /// On its own this is the normal state of a manual channel. Paired with the
    /// publishing posture it answers the question an operator actually asks:
    /// "I approved everything and set autopilot — why has nothing published?"
    reddit_drafts_waiting: i64,
    /// Reddit drafts that failed in the last day, with the distinct reasons.
    ///
    /// `community_posts.error_message` is written on every failure and read by
    /// nothing. A failed draft is not recoverable by the brain — the parent
    /// action is terminal and the seven-day subreddit cooldown stops it drafting
    /// the community again — so the reason is the only thing that tells an
    /// operator whether to requeue the content or fix the account.
    reddit_drafts_failed: Option<String>,
    /// Approvals cancelled unanswered in the last week, and the soonest deadline
    /// still outstanding in hours.
    ///
    /// An approval is inserted with `approval_expires_at = now() + 72 hours` and
    /// the claim sweep cancels it at that point with
    /// `last_error_kind = 'approval_expired'`. Every one of those is a proposal
    /// the brain made, an operator never saw in time, and the system threw away.
    ///
    /// It is the one loss that scales with the operator being the bottleneck,
    /// which is the state this deployment is in — the queue is the throughput
    /// limit, and the queue empties itself every three days whether or not
    /// anybody looked. None of the other eighteen conditions watches it: they
    /// watch executors, feeds, drafts and the brain, all of which are working
    /// when this happens.
    approvals_expired_7d: i64,
    /// Hours until the next outstanding approval expires. `None` when nothing is
    /// awaiting approval.
    ///
    /// Reported alongside the expiry count so the alarm can say what is about to
    /// go as well as what already went. A count of past losses tells an operator
    /// they were too slow; a deadline tells them what to do today.
    hours_to_next_approval_expiry: Option<i64>,
    /// Approvals currently outstanding, whatever their deadline.
    approvals_outstanding: i64,
    /// Asks whose approval window closed more than two hours ago and still
    /// read `awaiting_approval`. The deadline is the contract: at
    /// `approval_expires_at` the row must leave the queue as
    /// `cancelled`/`approval_expired`. The claim path sweeps its workspace
    /// every cycle and retention sweeps globally every hour, so anything
    /// past two intervals means *no* sweep is running — a parked tenant
    /// before retention existed, or a worker that stopped claiming and is
    /// not running retention either.
    unswept_lapsed_approvals: i64,
    /// Queued actions parked waiting on an executor capability nobody
    /// advertises right now.
    ///
    /// The park sweep writes `last_error_kind='awaiting_executor'`; an action
    /// carrying it has already tried dispatch once and found no live
    /// advertisement. It unparks itself when the capability returns, so a
    /// parked row is a current need, not history.
    awaiting_executor_actions: i64,
    /// Actions the grace sweep cancelled in the last week with
    /// `last_error_kind='no_executor'`. Each is work the brain decided,
    /// nobody executed, and the system threw away — the parked state above
    /// made permanent.
    no_executor_cancelled_7d: i64,
    /// Distinct action kinds across both populations — the work classes that
    /// cannot run. The capability an action needs is derived from its payload
    /// in Rust (`executor_capability_for_payload`); reproducing that mapping
    /// in SQL would be a second copy to drift, so the finding names the kind
    /// and lets the operator map it to the executor that went dark.
    unclaimed_action_kinds: Option<String>,
    /// Policies the autonomy guardrail demoted and that are still inside
    /// their guard window, as `context until <UTC time>` joined by `, `;
    /// `None` when none is. The demotion emits
    /// `crowdrelay.autopilot.authority_demoted`, which the n8n router has no
    /// handler for and refuses with 422 — two were dead on 2026-09-26 — so
    /// this is the operator's only notice that the brain now asks first.
    guarded_policies: Option<String>,
    /// Actions a person approved that then failed, in the last 24 hours.
    /// On 2026-09-28 the operator approved 39 letters and 38 failed at
    /// dispatch within five minutes; no condition watched approved work, so
    /// nothing said so.
    approved_failed_24h: i64,
    /// `action_kind: error_kind ×count` for those failures, joined by `, `.
    approved_failed_summary: Option<String>,
    /// Drafts waiting on a Reddit session (pending or deferred).
    reddit_posting_demand: i64,
    /// Count of credential rows eligible to establish a session — the same
    /// eligibility the agents service's `getRedditCredentials` applies.
    reddit_session_usable: i64,
    /// The credential row's status (`active`/`cooldown`/`invalid`), or NULL
    /// when no `reddit-browser` credential exists at all.
    reddit_credential_status: Option<String>,
    reddit_credential_error: Option<String>,
    /// Lifetime decisions, and how many observations the causal posterior has.
    ///
    /// The brain checkpoints its causal model to `brain_state`, and that
    /// model carries its own observation count. Reading it answers "has any
    /// evidence ever corrected this belief" directly, rather than inferring it
    /// from a proxy like the resolved-evidence count.
    ///
    /// `None` means no checkpoint exists yet, which for a workspace that has
    /// made decisions means the same thing as zero.
    decisions_total: i64,
    causal_observations: Option<i64>,
    /// Drops inside their surge window whose fan-out produced nothing a fan
    /// can see, as one JSON object per source: title, age, and each surge
    /// lane's action status (`instagram=awaiting_approval,email=succeeded`).
    ///
    /// The window matters because it is where a new video earns its reach —
    /// on 2026-09-28 a premiere spent its whole first day with fifty-nine
    /// artifact requests "succeeding" and zero posts on any owned channel,
    /// and nothing said so. The stall grace (the oldest surge action older
    /// than three hours) covers executor poll lag and a normal approval
    /// cadence; past it, a lane still parked is the finding — the lane
    /// states in the detail say whether the fix is an approval click or an
    /// executor repair.
    stalled_drops: Option<Value>,
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

/// How many lifetime decisions a workspace makes before an untouched causal
/// prior is a fault rather than a young system.
///
/// A workspace that has decided a hundred times has had a hundred chances to
/// learn something. Below that, "no observations yet" is a system that has not
/// run, which is a different statement and not worth waking anybody for.
const DECISIONS_BEFORE_LEARNING_EXPECTED: i64 = 100;

include!("ops_watchdog/snapshot.rs");

include!("ops_watchdog/conditions.rs");

async fn load_states(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<HashMap<String, AlertState>, sqlx::Error> {
    let rows = sqlx::query_as::<_, AlertState>(
        r#"
        SELECT alert_key, active, last_alerted_at
        FROM ops_alert_state
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
        INSERT INTO ops_alert_state (
            workspace_id, alert_key, severity, summary, active,
            first_seen_at, last_seen_at, last_alerted_at, details
        ) VALUES ($1,$2,$3,$4,true,$5,$5,CASE WHEN $7 THEN $5 ELSE NULL END,$6)
        ON CONFLICT (workspace_id, alert_key) DO UPDATE
        SET severity=EXCLUDED.severity,
            summary=EXCLUDED.summary,
            active=true,
            first_seen_at=CASE
                WHEN ops_alert_state.active THEN ops_alert_state.first_seen_at
                ELSE EXCLUDED.first_seen_at
            END,
            last_seen_at=EXCLUDED.last_seen_at,
            last_alerted_at=CASE
                WHEN $7 THEN EXCLUDED.last_alerted_at
                ELSE ops_alert_state.last_alerted_at
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
        UPDATE ops_alert_state
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
