//! Background evaluator/executor for deterministic CrowdRelay Autopilot actions.

use std::time::Duration;

use crowdrelay_application::{
    RepositoryError,
    autopilot::{
        AutopilotActionRepository, AutopilotContext, AutopilotDecisionRepository, AutopilotError,
        AutopilotFirstPartyGrowthMetrics, AutopilotMeasurementKind, AutopilotMeasurementRepository,
        AutopilotPlayOutcomeRepository, AutopilotPolicyConfig, AutopilotReplyTriageRepository,
        AutopilotWaveOutcomeRepository, EvaluateAutopilot, ORG_ATTENTION_BUDGET_ERROR_KIND,
        assess_measurement_effect, assess_play_claim, assess_wave_claim,
    },
};
use crowdrelay_domain::{
    WorkspaceId, join_ask::JoinAskHold, play_measurement::PlayMeasurementPolicy,
};
use crowdrelay_infra::autopilot::{CycleTrigger, PostgresAutopilotRepository};
use sqlx::postgres::PgListener;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};

/// NOTIFY channel an operator's "run a cycle now" request arrives on.
///
/// A manual run wakes this loop rather than executing anywhere else, so it
/// takes the identical path as a scheduled tick — including the 24-hour action
/// quota, which is enforced in the same transaction that writes an action.
/// There is deliberately no second code path to keep in step, and therefore no
/// way for the button to outrun the guardrails.
pub const AUTOPILOT_CYCLE_CHANNEL: &str = "autopilot_cycle";

const ACTION_BATCH_SIZE: u32 = 32;
const MEASUREMENT_BATCH_SIZE: u32 = 16;
/// Play outcomes settle once per play, weeks after the campaign ran. A small
/// batch is right: there is never a backlog unless something has been broken
/// for a month, and in that case draining it slowly is the safer failure.
const PLAY_OUTCOME_BATCH_SIZE: u32 = 8;
/// Wave outcomes settle once per wave, three weeks after the pitches were
/// released. Same small batch: a backlog means something has been broken for
/// weeks, and draining it slowly is the safer failure.
const WAVE_OUTCOME_BATCH_SIZE: u32 = 8;
const REPLY_TRIAGE_BATCH_SIZE: u32 = 50;

/// Stable identifiers for the phases of one autopilot cycle.
///
/// These are recorded on the cycle row and read by an operator, so they are
/// part of a contract: renaming one silently re-labels history. They are
/// deliberately not the log messages, which are prose and change freely.
mod phase {
    pub const GROWTH_METRIC_CAPTURE: &str = "growth_metric_capture";
    pub const EVALUATION: &str = "evaluation";
    pub const TEAM_HANDOFF_RECONCILIATION: &str = "team_handoff_reconciliation";
    /// The roster brief's issue attempt — one org-wide artifact plus its
    /// handoffs, raced once per member workspace and won by exactly one.
    pub const ROSTER_BRIEF_ISSUE: &str = "roster_brief_issue";
    pub const NO_EXECUTOR_SWEEP: &str = "no_executor_sweep";
    pub const ABANDONED_CLAIM_SWEEP: &str = "abandoned_claim_sweep";
    pub const ACTION_EXECUTION: &str = "action_execution";
    pub const ACTION_CLAIM: &str = "action_claim";
    pub const MEASUREMENT_RESOLUTION: &str = "measurement_resolution";
    pub const MEASUREMENT_CLAIM: &str = "measurement_claim";
    pub const PLAY_OUTCOME_RESOLUTION: &str = "play_outcome_resolution";
    pub const PLAY_OUTCOME_CLAIM: &str = "play_outcome_claim";
    pub const WAVE_OUTCOME_RESOLUTION: &str = "wave_outcome_resolution";
    pub const WAVE_OUTCOME_CLAIM: &str = "wave_outcome_claim";
    pub const REPLY_CLASSIFICATION: &str = "reply_classification";
    pub const REPLY_TRIAGE_CLAIM: &str = "reply_triage_claim";
    /// The one phase a park-skipped cycle can fail. The park read fails closed
    /// — an unreadable park flag is treated as parked — so a cycle degraded on
    /// this alone took no action at all, which is a different situation from a
    /// phase falling over mid-cycle.
    pub const PARK_CHECK: &str = "park_check";
    /// Re-tuning worker call parameters from the LLM call telemetry tail.
    /// Its failure leaves the previous tuning row in effect — a degraded
    /// cycle tunes nothing, it never untunes.
    pub const LLM_TUNING: &str = "llm_tuning";
    /// The fan-source attribution snapshot — persisting what the brain's
    /// evidence ledger already computed. Purely observational: a failure
    /// here loses one operator reading, never an action or a measurement.
    pub const FAN_SOURCE_SNAPSHOT: &str = "fan_source_snapshot";
}

/// Which phases of a cycle fell over.
///
/// This replaced a single `phase_failed` boolean that eighteen call sites could
/// set. The boolean was enough to mark the cycle `degraded` and not enough to
/// say anything else: production ran 296 cycles in 24 hours with 40 degraded,
/// and answering "which phase" meant grepping worker logs by timestamp — so the
/// answer expired with the logs. A 13% degraded rate is either phase isolation
/// doing its job on transient errors or one phase broken every cycle, and those
/// call for opposite responses.
///
/// A set, because a phase that iterates (actions, measurements, replies) can
/// fail on many items in one cycle and that is still one broken phase. Ordered,
/// so the recorded value does not change with iteration order.
///
/// The value paired with each phase is the error kind it first failed with, in
/// the `repository_error_kind` vocabulary. First wins for the same reason the
/// phase itself is a set: one broken phase is one entry, whatever the iteration
/// count behind it was.
#[derive(Clone, Debug, Default)]
struct DegradedPhases(std::collections::BTreeMap<&'static str, &'static str>);

impl DegradedPhases {
    fn failed(&mut self, phase: &'static str, kind: &'static str) {
        self.0.entry(phase).or_insert(kind);
    }

    fn any(&self) -> bool {
        !self.0.is_empty()
    }

    /// The value written to the cycle row. Empty means no phase failed, which
    /// is a different statement from the NULL a pre-column cycle carries.
    fn recorded(&self) -> Vec<String> {
        self.0.keys().map(|phase| (*phase).to_owned()).collect()
    }

    /// Phase → error kind, written beside `recorded()` so "which phase broke"
    /// also answers "with what" after the worker log rolls. An empty object
    /// for a clean cycle, matching the empty `degraded_phases` array.
    fn recorded_errors(&self) -> serde_json::Value {
        self.0
            .iter()
            .map(|(phase, kind)| ((*phase).to_owned(), (*kind).into()))
            .collect::<serde_json::Map<String, serde_json::Value>>()
            .into()
    }

    /// The pair `close_cycle_run` writes — one value so the phases and the
    /// kinds cannot disagree about which map they came from.
    fn cycle_degradation(&self) -> crowdrelay_infra::autopilot::CycleDegradation {
        crowdrelay_infra::autopilot::CycleDegradation {
            phases: self.recorded(),
            errors: self.recorded_errors(),
        }
    }
}

/// What one cycle produced, for the record that describes it.
#[derive(Clone, Debug, Default)]
struct CycleObservation {
    /// Which phases fell over while the others completed. Not a failed cycle:
    /// the phases are isolated so that one failing cannot stop
    /// already-authorized work, and calling that failure would teach an
    /// operator to ignore the word. Empty means the cycle was clean.
    degraded: DegradedPhases,
    /// The North Star as the evaluation phase read it, in whatever metric the
    /// tenant has chosen. `None` when that phase did not get far enough to
    /// take a reading.
    north_star: Option<u32>,
    /// Why the portfolio selected nothing, in the brain's own words. The
    /// system may do nothing and say so — but only if the reason survives
    /// past the worker log it was first written to.
    wait_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AutopilotWorker {
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
}

impl AutopilotWorker {
    #[must_use]
    pub fn new(
        repository: PostgresAutopilotRepository,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
    ) -> Self {
        Self {
            repository,
            workspace_id,
            poll_interval,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticks = interval(self.poll_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // The listener is optional on purpose. Losing it costs the manual
        // trigger, not the scheduled cycle, so a listener that will not connect
        // must not take the autopilot down with it.
        let mut listener = match PgListener::connect_with(self.repository.pool()).await {
            Ok(mut listener) => match listener.listen(AUTOPILOT_CYCLE_CHANNEL).await {
                Ok(()) => Some(listener),
                Err(error) => {
                    tracing::warn!(error = %error, "autopilot cycle listener could not subscribe; scheduled cycles continue");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(error = %error, "autopilot cycle listener unavailable; scheduled cycles continue");
                None
            }
        };

        loop {
            let notified = async {
                match listener.as_mut() {
                    Some(listener) => listener.recv().await.map(|_| ()).map_err(|error| {
                        tracing::warn!(error = %error, "autopilot cycle listener dropped");
                    }),
                    // No listener: never resolve, so `select!` falls through to
                    // the tick arm exactly as it did before.
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                result = notified => {
                    if result.is_err() {
                        listener = None;
                        continue;
                    }
                    tracing::info!("CrowdRelay Autopilot cycle requested by operator");
                    self.run_recorded_cycle(CycleTrigger::Requested).await;
                }
                _ = ticks.tick() => {
                    self.run_recorded_cycle(CycleTrigger::Scheduled).await;
                }
            }
        }
    }

    /// Runs one cycle and records that it happened.
    ///
    /// The four phases are isolated on purpose -- one failing must not block the
    /// others -- and that is exactly why nothing tied a cycle together: each
    /// phase logged its own line and `phase_failed` collapsed all of them into
    /// one boolean. Asking "which cycle produced that decision, and what else
    /// did that cycle do" meant correlating timestamps across four tables and a
    /// log, which is how a brain fixating on a dead channel went unnoticed for
    /// two weeks.
    ///
    /// The cycle id also enters a tracing span, so every line the cycle emits
    /// carries it and the log can be filtered to one run.
    ///
    /// Recording never gates the work: an unopened record still runs the cycle,
    /// because losing the note of what the brain did must not cost the doing.
    async fn run_recorded_cycle(&self, trigger: CycleTrigger) {
        let started = OffsetDateTime::now_utc();

        // Park check: if the tenant is parked, the cycle opens, records that
        // it was skipped, and closes — no evaluation, no actions, no
        // measurements. The flag lives on the growth envelope row, set by the
        // Control Plane on park and cleared on resume. A read failure must not
        // gate the cycle: the agent stays live if the park check itself breaks.
        let mut park_check_error = None;
        let parked = match AutopilotDecisionRepository::load_growth_envelope(
            &self.repository,
            self.workspace_id,
            started,
        )
        .await
        {
            Ok((envelope, _)) => envelope.parked,
            Err(error) => {
                // Fail closed: if we cannot determine the park state, treat
                // the tenant as parked so autonomous actions are not taken
                // while an operator may believe growth is paused.
                tracing::warn!(error = %error, "park check failed; treating tenant as parked (fail closed)");
                park_check_error = Some(repository_error_kind(error));
                true
            }
        };
        if parked {
            tracing::info!("autopilot cycle skipped — tenant is parked");
            // A park-skipped cycle ran no phase, so the only thing that can
            // degrade it is the park check itself.
            let mut skipped = DegradedPhases::default();
            if let Some(kind) = park_check_error {
                skipped.failed(phase::PARK_CHECK, kind);
            }
            let cycle_id = crowdrelay_infra::autopilot::open_cycle_run(
                self.repository.pool(),
                self.workspace_id,
                trigger,
                started,
            )
            .await;
            if let Some(cycle_id) = cycle_id {
                crowdrelay_infra::autopilot::close_cycle_run(
                    self.repository.pool(),
                    self.workspace_id,
                    cycle_id,
                    &skipped.cycle_degradation(),
                    OffsetDateTime::now_utc(),
                    None,
                    None,
                )
                .await;
            }
            return;
        }

        let cycle_id = crowdrelay_infra::autopilot::open_cycle_run(
            self.repository.pool(),
            self.workspace_id,
            trigger,
            started,
        )
        .await;
        let span = tracing::info_span!(
            "autopilot_cycle",
            cycle_id = cycle_id.map(|id| id.to_string()).unwrap_or_default()
        );
        let observed = {
            let _entered = span.enter();
            self.run_once(started).await
        };
        if let Some(cycle_id) = cycle_id {
            crowdrelay_infra::autopilot::close_cycle_run(
                self.repository.pool(),
                self.workspace_id,
                cycle_id,
                &observed.degraded.cycle_degradation(),
                OffsetDateTime::now_utc(),
                observed.north_star,
                observed.wait_reason.as_deref(),
            )
            .await;
        }
    }

    /// Runs one cycle and reports what the record of it should say.
    ///
    /// Not a `Result`: a failed phase does not fail the cycle. The phases are
    /// isolated on purpose, so a cycle in which one fell over and the rest
    /// completed is degraded, and the reading the evaluation phase took is
    /// still a real reading. Collapsing that into an error discarded it —
    /// putting a gap in the series the brain assesses itself from at exactly
    /// the moment something was going wrong.
    async fn run_once(&self, now: OffsetDateTime) -> CycleObservation {
        // Evaluation, execution and delayed measurement are intentionally isolated.
        // A context-specific query failure must never block already-authorized work
        // or evidence collection from a previous cycle.
        let mut degraded = DegradedPhases::default();
        let mut north_star_observed = None;
        let mut wait_reason = None;

        // Recording first-party observations runs before evaluation so a cycle
        // reasons about the newest evidence it can. It is a separate phase
        // because a metric write failing must not stop already-authorized work:
        // the evaluator simply sees a slightly older window.
        match self
            .repository
            .materialize_first_party_growth_metrics(self.workspace_id, now)
            .await
        {
            Ok(report) if report.points_recorded > 0 || report.series_retired > 0 => {
                tracing::info!(
                    series_tracked = report.series_tracked,
                    series_retired = report.series_retired,
                    points_recorded = report.points_recorded,
                    "CrowdRelay recorded first-party growth observations"
                );
            }
            Ok(_) => {}
            Err(error) => {
                degraded.failed(phase::GROWTH_METRIC_CAPTURE, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay first-party growth metric capture failed");
            }
        }

        let evaluator = EvaluateAutopilot::new(&self.repository, self.workspace_id);
        match evaluator.execute(now).await {
            Ok(report) => {
                north_star_observed = report.north_star_observed;
                // A quiet cycle owes its reason in full: the portfolio's WAIT
                // math and, when the watcher found nothing, the missing
                // material itself. Both ride the same column — an operator
                // reading "why did nothing happen" needs the two halves, not
                // whichever one got stored first. A candidate the 24h quota
                // ate is a third quiet: it leaves no decision row anywhere,
                // so the context that produced it is named here or nowhere.
                let mut quiet_parts: Vec<String> = [
                    report.supply_wait_reason.clone(),
                    report.gi_wait_reason.clone(),
                ]
                .into_iter()
                .flatten()
                .collect();
                quiet_parts.extend(
                    report
                        .context_activity
                        .values()
                        .filter(|activity| activity.throttled > 0)
                        .map(|activity| {
                            format!(
                                "{} asked {} time(s); its 24h quota was already spent",
                                activity.context.as_str(),
                                activity.throttled,
                            )
                        }),
                );
                wait_reason = (!quiet_parts.is_empty()).then(|| quiet_parts.join("; "));
                tracing::info!(
                    decisions = report.decisions,
                    actions_enqueued = report.actions_enqueued,
                    actions_throttled = report.actions_throttled,
                    plays_started = report.plays_started,
                    play_steps_skipped = report.play_steps_skipped,
                    play_steps_delivered = report.play_steps_delivered,
                    plays_completed = report.plays_completed,
                    north_star = ?report.north_star_observed,
                    gi_candidates = report.gi_candidates,
                    gi_wait_reason = ?report.gi_wait_reason,
                    gi_dispatch_log = ?report.gi_dispatch_log,
                    context_activity = ?report.context_activity,
                    "autopilot cycle report"
                );
                // A prerequisite gate that drops candidates silently reads
                // exactly like a brain with nothing to say. Production
                // discovered 119 communities, joined none — the join executor
                // is in manual mode — and every community candidate was gated
                // out with no decision row anywhere. The Reddit acquisition
                // channel looked idle when it was blocked on one manual step.
                //
                // WARN, not INFO: this is work waiting on a person.
                if !report.blocked_on_membership.is_empty() {
                    let auto_join_enabled =
                        std::env::var("CROWDRELAY_COMMUNITY_AUTO_JOIN").as_deref() == Ok("true");
                    let waiting: u32 = report
                        .blocked_on_membership
                        .iter()
                        .map(|(_, count)| *count)
                        .sum();
                    tracing::warn!(
                        communities = report.blocked_on_membership.len(),
                        posts_waiting = waiting,
                        join_first = ?report
                            .blocked_on_membership
                            .iter()
                            .take(5)
                            .collect::<Vec<_>>(),
                        auto_join = auto_join_enabled,
                        // The remedy depends on whether auto-join is already
                        // on. Telling an operator to set a flag they set
                        // already sends them to the wrong place: when it is
                        // on, the joins are being attempted and failing, and
                        // the reason is in `failed to join community`.
                        "growth blocked: posts are waiting on communities nobody has joined"
                    );
                }
                // Same reason as the membership gate above: a configured
                // join-ask platform that stays quiet is work waiting on a
                // person (no connection, no photo, no site URL), and a
                // silent skip reads exactly like a detector that never ran.
                //
                // Split by level, because the cold-start case now reaches
                // here too and it arrives on every poll. A tenant that never
                // wrote its ask is held on `NoVariants` alone — ordinary,
                // expected, and already reported where somebody can act on
                // it (`join_ask_readiness` on the attention board). Warning
                // about it each tick would bury the case that is genuinely
                // odd: a tenant that *did* write its ask and still cannot
                // post.
                if !report.join_ask_held.is_empty() {
                    let never_configured = report
                        .join_ask_held
                        .iter()
                        .all(|(_, hold)| matches!(hold, JoinAskHold::NoVariants));
                    if never_configured {
                        tracing::debug!(
                            held = ?report.join_ask_held,
                            "join-ask not set up for this workspace yet"
                        );
                    } else {
                        tracing::warn!(
                            held = ?report.join_ask_held,
                            "join-ask platforms held this cycle"
                        );
                    }
                }
            }
            Err(error) => {
                degraded.failed(phase::EVALUATION, autopilot_error_kind(&error));
                tracing::warn!(error = %error, "CrowdRelay Autopilot evaluation failed");
            }
        }

        match self
            .repository
            .reconcile_team_handoffs(self.workspace_id, now)
            .await
        {
            Ok(count) if count > 0 => tracing::info!(count, "assigned CrowdRelay human handoffs"),
            Ok(_) => {}
            Err(error) => {
                degraded.failed(
                    phase::TEAM_HANDOFF_RECONCILIATION,
                    repository_error_kind(error),
                );
                tracing::warn!(error = %error, "CrowdRelay team handoff reconciliation failed");
            }
        }
        // The roster weekly brief rides the same handoff rail as the daily
        // briefing, one sweep later so a handoff failure cannot take the
        // brief down with it — and vice versa.
        match self
            .repository
            .issue_roster_weekly_briefs(self.workspace_id, now)
            .await
        {
            Ok(count) if count > 0 => {
                tracing::info!(count, "issued CrowdRelay roster weekly brief")
            }
            Ok(_) => {}
            Err(error) => {
                degraded.failed(phase::ROSTER_BRIEF_ISSUE, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay roster weekly brief issue failed");
            }
        }
        match self
            .repository
            .cancel_unexecutable_actions(self.workspace_id, now)
            .await
        {
            Ok(count) if count > 0 => {
                tracing::info!(count, "cancelled CrowdRelay actions with no live executor")
            }
            Ok(_) => {}
            Err(error) => {
                degraded.failed(phase::NO_EXECUTOR_SWEEP, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay no-executor sweep failed");
            }
        }

        // An executor that dies mid-action leaves its claim open forever, and
        // every reading of "what is in flight" then counts work that stopped.
        // Settled from the action's own terminal status, so this reconciles
        // rather than guesses.
        match self
            .repository
            .settle_abandoned_execution_claims(self.workspace_id, now)
            .await
        {
            Ok(_) => {}
            Err(error) => {
                degraded.failed(phase::ABANDONED_CLAIM_SWEEP, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay abandoned-claim sweep failed");
            }
        }

        match self
            .repository
            .claim_due_autonomous_actions(self.workspace_id, ACTION_BATCH_SIZE, now)
            .await
        {
            Ok(actions) => {
                for action in actions {
                    if let Err(error) = self
                        .repository
                        .execute_action(self.workspace_id, &action, OffsetDateTime::now_utc())
                        .await
                    {
                        let error_kind = repository_error_kind(error);
                        degraded.failed(phase::ACTION_EXECUTION, error_kind);
                        let retryable = repository_error_retryable(error);
                        tracing::warn!(
                            action_id = %action.id,
                            action_kind = action.payload.action_kind(),
                            error_kind,
                            "CrowdRelay Autopilot action failed"
                        );
                        let _ = self
                            .repository
                            .fail_action(
                                self.workspace_id,
                                action.id,
                                error_kind,
                                retryable,
                                OffsetDateTime::now_utc(),
                            )
                            .await
                            .inspect_err(|e| {
                                tracing::error!(
                                    action_id = %action.id,
                                    error = %e,
                                    "failed to mark CrowdRelay action as failed — action may remain in-flight"
                                );
                            })
                            .ok();
                    }
                }
            }
            Err(error) => {
                degraded.failed(phase::ACTION_CLAIM, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay Autopilot action claim failed");
            }
        }

        // Delayed measurements deliberately run after action execution. They never
        // influence the side effect that created them; they only produce immutable
        // evidence for later policy calibration.
        match self
            .repository
            .claim_due_measurements(self.workspace_id, MEASUREMENT_BATCH_SIZE, now)
            .await
        {
            Ok(measurements) => {
                let claimed = measurements.len();
                let mut succeeded = 0u32;
                let mut failed = 0u32;
                for measurement in measurements {
                    let observed_at = OffsetDateTime::now_utc();
                    // Harm is observed before the primary metric: it exists
                    // whether or not the metric is readable — a cancelled
                    // event's measurement abandons while its harm is real.
                    // `None` is a failed observation, distinct from a clean
                    // zero reading: the completion writes no harm keys for
                    // it, so a broken collector cannot teach "no harm" or
                    // overwrite what an earlier attempt landed.
                    let harm = self
                        .repository
                        .observe_action_harm(self.workspace_id, &measurement, observed_at)
                        .await
                        .inspect_err(|error| {
                            tracing::warn!(
                                measurement_id = %measurement.id,
                                error = %error,
                                "CrowdRelay Autopilot harm observation failed — resolving without it"
                            );
                        })
                        .ok();
                    let assess_harm = harm.unwrap_or_default();
                    let result = async {
                        let observed = self
                            .repository
                            .observe_measurement(self.workspace_id, &measurement, observed_at)
                            .await?;
                        let effect =
                            assess_measurement_effect(&measurement, observed, &assess_harm)
                                .ok_or(RepositoryError::Unexpected)?;
                        self.repository
                            .complete_measurement(
                                self.workspace_id,
                                &measurement,
                                observed,
                                effect,
                                harm.as_ref(),
                                observed_at,
                            )
                            .await
                    }
                    .await;

                    match result {
                        Ok(()) => succeeded += 1,
                        Err(error) => {
                            failed += 1;
                            let error_kind = repository_error_kind(error);
                            degraded.failed(phase::MEASUREMENT_RESOLUTION, error_kind);
                            let retryable = repository_error_retryable(error);
                            tracing::warn!(
                                measurement_id = %measurement.id,
                                measurement_kind = measurement.kind.as_str(),
                                error_kind,
                                "CrowdRelay Autopilot delayed effect measurement failed"
                            );
                            // `harm` rides along: a terminal failure merges
                            // its keys in the same transaction that resolves
                            // readiness, so the evidence row closes with the
                            // harm it observed already on it.
                            let _ = self
                                .repository
                                .fail_measurement(
                                    self.workspace_id,
                                    &measurement,
                                    error_kind,
                                    retryable,
                                    harm.as_ref(),
                                    OffsetDateTime::now_utc(),
                                )
                                .await
                                .inspect_err(|e| {
                                    tracing::error!(
                                        measurement_id = %measurement.id,
                                        error = %e,
                                        "failed to mark CrowdRelay measurement as failed — measurement may remain in-flight"
                                    );
                                })
                                .ok();
                        }
                    }
                }
                if claimed > 0 {
                    tracing::info!(
                        claimed,
                        succeeded,
                        failed,
                        "CrowdRelay Autopilot measurement phase completed"
                    );
                }
            }
            Err(error) => {
                degraded.failed(phase::MEASUREMENT_CLAIM, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay Autopilot measurement claim failed");
            }
        }

        // Play outcomes settle last, and settle even when the plays context is
        // switched off. Measuring what already happened is not acting on it,
        // and a campaign that ran before the operator paused the agent still
        // deserves an honest answer about what it did.
        let measurement_policy = self.play_measurement_policy().await;
        match self
            .repository
            .claim_due_play_outcomes(self.workspace_id, PLAY_OUTCOME_BATCH_SIZE, now)
            .await
        {
            Ok(outcomes) => {
                for outcome in outcomes {
                    let settled_at = OffsetDateTime::now_utc();
                    let result = async {
                        let observation = self
                            .repository
                            .observe_play_outcome(self.workspace_id, &outcome, settled_at)
                            .await?;
                        let verdict = assess_play_claim(&outcome, &observation, measurement_policy);
                        self.repository
                            .complete_play_outcome(
                                self.workspace_id,
                                &outcome,
                                &observation,
                                verdict,
                                settled_at,
                            )
                            .await
                    }
                    .await;

                    if let Err(error) = result {
                        let error_kind = repository_error_kind(error);
                        degraded.failed(phase::PLAY_OUTCOME_RESOLUTION, error_kind);
                        let retryable = repository_error_retryable(error);
                        tracing::warn!(
                            play_id = %outcome.play_id,
                            claim = outcome.claim.as_str(),
                            error_kind,
                            "CrowdRelay play outcome measurement failed"
                        );
                        let _ = self
                            .repository
                            .fail_play_outcome(
                                self.workspace_id,
                                outcome.id,
                                error_kind,
                                retryable,
                                OffsetDateTime::now_utc(),
                            )
                            .await
                            .inspect_err(|e| {
                                tracing::error!(
                                    play_id = %outcome.play_id,
                                    error = %e,
                                    "failed to mark CrowdRelay play outcome as failed — outcome may remain in-flight"
                                );
                            })
                            .ok();
                    }
                }
            }
            Err(error) => {
                degraded.failed(phase::PLAY_OUTCOME_CLAIM, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay play outcome claim failed");
            }
        }

        // Wave outcomes settle last, and settle even when the outreach context
        // is switched off — for the same reason play outcomes do: measuring
        // what already happened is not acting on it.
        match self
            .repository
            .claim_due_wave_outcomes(self.workspace_id, WAVE_OUTCOME_BATCH_SIZE, now)
            .await
        {
            Ok(outcomes) => {
                for outcome in outcomes {
                    let settled_at = OffsetDateTime::now_utc();
                    let result = async {
                        let observation = self
                            .repository
                            .observe_wave_outcome(self.workspace_id, &outcome, settled_at)
                            .await?;
                        let verdict = assess_wave_claim(&outcome, &observation);
                        self.repository
                            .complete_wave_outcome(
                                self.workspace_id,
                                &outcome,
                                &observation,
                                verdict,
                                settled_at,
                            )
                            .await
                    }
                    .await;

                    if let Err(error) = result {
                        let error_kind = repository_error_kind(error);
                        degraded.failed(phase::WAVE_OUTCOME_RESOLUTION, error_kind);
                        let retryable = repository_error_retryable(error);
                        tracing::warn!(
                            wave_id = %outcome.wave_id,
                            target_kind = outcome.target_kind.as_str(),
                            error_kind,
                            "CrowdRelay wave outcome measurement failed"
                        );
                        let _ = self
                            .repository
                            .fail_wave_outcome(
                                self.workspace_id,
                                outcome.id,
                                error_kind,
                                retryable,
                                OffsetDateTime::now_utc(),
                            )
                            .await
                            .inspect_err(|e| {
                                tracing::error!(
                                    wave_id = %outcome.wave_id,
                                    error = %e,
                                    "failed to mark CrowdRelay wave outcome as failed — outcome may remain in-flight"
                                );
                            })
                            .ok();
                    }
                }
            }
            Err(error) => {
                degraded.failed(phase::WAVE_OUTCOME_CLAIM, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay wave outcome claim failed");
            }
        }

        // Reply triage settles last. Replies with `Received` disposition are
        // classified by the first-party domain classifier, and the result is
        // recorded. `NeedsHuman` classifications surface via the operator
        // brief. This runs even when outreach is switched off, because
        // classifying a reply that already arrived is measurement, not action.
        match self
            .repository
            .load_replies_needing_triage(self.workspace_id, REPLY_TRIAGE_BATCH_SIZE)
            .await
        {
            Ok(replies) => {
                for reply in replies {
                    let classified_at = OffsetDateTime::now_utc();
                    let classification = match reply.target_kind {
                        crowdrelay_application::autopilot::ReplyTargetKind::Outreach(kind) => {
                            let input =
                                crowdrelay_domain::reply_triage::ReplyClassificationInput {
                                    reply_text: &reply.reply_text,
                                    target_kind: kind,
                                    previous_disposition: reply.previous_disposition,
                                };
                            crowdrelay_domain::reply_triage::classify_reply(&input)
                        }
                        // A negotiation reply is always a human's call — the
                        // operator filed the disposition with the reply, and
                        // the triage row's job is to carry the proposed terms
                        // the reader extracted, not a second disposition.
                        crowdrelay_application::autopilot::ReplyTargetKind::BookingCounterparty => {
                            crowdrelay_domain::reply_triage::ReplyClassification::NeedsHuman {
                                reason: crowdrelay_domain::reply_triage::HumanReviewReason::NegotiationReply,
                                confidence: crowdrelay_domain::autonomy::Confidence::saturating_from_basis_points(
                                    10_000,
                                ),
                            }
                        }
                    };
                    let result = crowdrelay_application::autopilot::ReplyTriageResult {
                        classification,
                        classified_at,
                    };
                    if let Err(error) = self
                        .repository
                        .record_reply_classification(self.workspace_id, reply.reply_id, &result)
                        .await
                    {
                        degraded.failed(phase::REPLY_CLASSIFICATION, repository_error_kind(error));
                        tracing::warn!(
                            reply_id = %reply.reply_id,
                            error = %error,
                            "CrowdRelay reply triage failed"
                        );
                    }
                }
            }
            Err(error) => {
                degraded.failed(phase::REPLY_TRIAGE_CLAIM, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay reply triage claim failed");
            }
        }

        // LLM call tuning runs last: it reads the call tail the agent service
        // wrote since the previous cycle and persists the next decision for
        // the runner to resolve. It is measurement-and-knob work, not
        // action — a failure here must never stall the cycle's real work,
        // and a stale tuning row is a smaller harm than a stalled one.
        match self.repository.retune_llm(self.workspace_id, now).await {
            Ok(tuning) if !tuning.reason.is_empty() => {
                tracing::info!(reason = %tuning.reason, "CrowdRelay re-tuned LLM call parameters");
            }
            Ok(_) => {}
            Err(error) => {
                degraded.failed(phase::LLM_TUNING, repository_error_kind(error));
                tracing::warn!(error = %error, "CrowdRelay LLM call tuning failed");
            }
        }

        // The fan-source snapshot runs last so the measurement phases above
        // land in the reading. The phase lives in its own module — `run`
        // returns the error kind exactly when the cycle should record it
        // degraded.
        if let Err(kind) =
            crate::fan_source_snapshot::run(&self.repository, self.workspace_id, now).await
        {
            degraded.failed(phase::FAN_SOURCE_SNAPSHOT, kind);
        }

        if degraded.any() {
            // Each phase has already logged what it hit. This line says the
            // cycle as a whole is degraded, which the previous one -- an
            // opaque `RepositoryError::Unexpected` raised here and logged by
            // the caller -- did not.
            tracing::warn!("CrowdRelay Autopilot cycle degraded: a phase failed");
        }
        CycleObservation {
            degraded,
            north_star: north_star_observed,
            wait_reason,
        }
    }

    /// The operator's reading policy, or the default when the context has none.
    ///
    /// A policy that cannot be read must not stop a measurement: the outcome
    /// would be silently deferred for ever, and an unmeasured play is exactly
    /// what this phase exists to prevent.
    async fn play_measurement_policy(&self) -> PlayMeasurementPolicy {
        self.repository
            .load_policies(self.workspace_id)
            .await
            .ok()
            .and_then(|policies| {
                policies
                    .into_iter()
                    .find_map(|policy| match (policy.context, policy.config) {
                        (AutopilotContext::Plays, AutopilotPolicyConfig::Plays(plays)) => {
                            Some(plays.measurement)
                        }
                        _ => None,
                    })
            })
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug)]
pub struct TeamEmailDispatchWorker {
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
}

impl TeamEmailDispatchWorker {
    #[must_use]
    pub fn new(
        repository: PostgresAutopilotRepository,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
    ) -> Self {
        Self {
            repository,
            workspace_id,
            poll_interval,
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
                    if let Err(error) = self.run_once(OffsetDateTime::now_utc()).await {
                        tracing::warn!(error = %error, "CrowdRelay team-email dispatch cycle failed");
                    }
                }
            }
        }
    }

    async fn run_once(&self, now: OffsetDateTime) -> Result<(), RepositoryError> {
        let mut phase_failed = false;

        // The reminder lane is retired — the daily briefing is the cadence.
        // This sweep only clears `next_reminder_at` rows stamped before the
        // retirement so a leftover schedule can never fire a mail.
        match self
            .repository
            .drain_team_reminder_schedule(self.workspace_id)
            .await
        {
            Ok(count) if count > 0 => {
                tracing::info!(count, "cleared retired CrowdRelay team reminder schedules")
            }
            Ok(_) => {}
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "CrowdRelay team reminder schedule drain failed");
            }
        }

        match self
            .repository
            .claim_due_team_email_actions(self.workspace_id, ACTION_BATCH_SIZE, now)
            .await
        {
            Ok(actions) => {
                for action in actions {
                    if let Err(error) = self
                        .repository
                        .execute_action(self.workspace_id, &action, OffsetDateTime::now_utc())
                        .await
                    {
                        phase_failed = true;
                        let error_kind = repository_error_kind(error);
                        let retryable = repository_error_retryable(error);
                        tracing::warn!(
                            action_id = %action.id,
                            action_kind = action.payload.action_kind(),
                            error_kind,
                            "CrowdRelay team-email action failed"
                        );
                        let _ = self
                            .repository
                            .fail_action(
                                self.workspace_id,
                                action.id,
                                error_kind,
                                retryable,
                                OffsetDateTime::now_utc(),
                            )
                            .await
                            .inspect_err(|e| {
                                tracing::error!(
                                    action_id = %action.id,
                                    error = %e,
                                    "failed to mark CrowdRelay team-email action as failed — action may remain in-flight"
                                );
                            })
                            .ok();
                    }
                }
            }
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "CrowdRelay team-email action claim failed");
            }
        }

        if phase_failed {
            Err(RepositoryError::Unexpected)
        } else {
            Ok(())
        }
    }
}

const fn repository_error_retryable(error: RepositoryError) -> bool {
    matches!(
        error,
        RepositoryError::Unavailable | RepositoryError::Unexpected
    )
}

/// The evaluator wraps repository errors and adds a serialization failure of
/// its own, so the EVALUATION phase's kind is resolved through both layers.
fn autopilot_error_kind(error: &AutopilotError) -> &'static str {
    match error {
        AutopilotError::Repository(inner) => repository_error_kind(*inner),
        AutopilotError::Serialization(_) => "serialization",
    }
}

pub(crate) fn repository_error_kind(error: RepositoryError) -> &'static str {
    match error {
        RepositoryError::Unavailable => "repository_unavailable",
        RepositoryError::NotFound => "subject_not_found",
        RepositoryError::Unexpected => "unexpected",
        // A named cause worth recording, matched rather than passed through.
        //
        // `ConflictBecause` also carries operator-facing sentences for HTTP
        // problem details, and this value is written to `last_error_kind`,
        // which is bounded at 96 characters. Forwarding the payload would put
        // an arbitrary-length string into a constrained column and fail the
        // write that records the failure. Every other conflict stays the
        // category it always was.
        RepositoryError::ConflictBecause(reason)
            if reason == AutopilotMeasurementKind::NEVER_PUBLISHED =>
        {
            AutopilotMeasurementKind::NEVER_PUBLISHED
        }
        // Same for the event-bound abandonments: "the show was cancelled" and
        // "the show never ticketed here" are why the measurement has no
        // outcome, and `state_changed` would say neither.
        RepositoryError::ConflictBecause(reason)
            if reason == AutopilotMeasurementKind::EVENT_CANCELLED =>
        {
            AutopilotMeasurementKind::EVENT_CANCELLED
        }
        RepositoryError::ConflictBecause(reason)
            if reason == AutopilotMeasurementKind::NO_ISSUED_PASSES =>
        {
            AutopilotMeasurementKind::NO_ISSUED_PASSES
        }
        // Release abandonments read the same way: "the release had no
        // tracked link" and "no series anchored the window" are why there is
        // no outcome, not a stale write.
        RepositoryError::ConflictBecause(reason)
            if reason == AutopilotMeasurementKind::NO_RELEASE_LINK
                || reason == AutopilotMeasurementKind::NO_RELEASE_SERIES_DATA
                || reason == AutopilotMeasurementKind::NO_TRACKED_LINK
                // A stack without an agent service names the same thing in
                // both directions: a measurement it can never read and a
                // dispatch it can never run are `no_agent_service`, not a
                // bare relation error and not `state_changed`.
                || reason == AutopilotMeasurementKind::NO_AGENT_SERVICE =>
        {
            reason
        }
        // Same shape as the measurement kind above: the contact governor
        // raises the roster's spent monthly attention share as a named
        // conflict, and "of the four things this roster wanted to tell this
        // person this month, three already went out" is not a stale write —
        // the lapsed/attention reads stay honest only if the kind says so.
        RepositoryError::ConflictBecause(reason) if reason == ORG_ATTENTION_BUDGET_ERROR_KIND => {
            ORG_ATTENTION_BUDGET_ERROR_KIND
        }
        RepositoryError::Conflict | RepositoryError::ConflictBecause(_) => "state_changed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The named conflicts that must reach `last_error_kind` verbatim rather
    /// than collapsing into `state_changed` — the funnel is the only thing
    /// keeping "the roster spent this person's monthly share" readable as
    /// itself on the action row.
    #[test]
    fn repository_error_kind_keeps_named_conflicts() {
        assert_eq!(
            repository_error_kind(RepositoryError::ConflictBecause(
                ORG_ATTENTION_BUDGET_ERROR_KIND
            )),
            "org_attention_budget"
        );
        assert_eq!(
            repository_error_kind(RepositoryError::ConflictBecause(
                AutopilotMeasurementKind::NEVER_PUBLISHED
            )),
            "dispatch_never_published"
        );
        assert_eq!(
            repository_error_kind(RepositoryError::Conflict),
            "state_changed"
        );
        assert_eq!(
            repository_error_kind(RepositoryError::ConflictBecause(
                "some other operator-facing reason"
            )),
            "state_changed"
        );
    }
}
