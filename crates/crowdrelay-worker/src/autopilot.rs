//! Background evaluator/executor for deterministic ViryaOS Autopilot actions.

use std::time::Duration;

use crowdrelay_application::{
    RepositoryError,
    autopilot::{
        AutopilotActionRepository, AutopilotContext, AutopilotDecisionRepository,
        AutopilotFirstPartyGrowthMetrics, AutopilotMeasurementRepository,
        AutopilotPlayOutcomeRepository, AutopilotPolicyConfig, AutopilotWaveOutcomeRepository,
        EvaluateAutopilot, assess_measurement_effect, assess_play_claim, assess_wave_claim,
    },
};
use crowdrelay_domain::{WorkspaceId, play_measurement::PlayMeasurementPolicy};
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};

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
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticks.tick() => {
                    if let Err(error) = self.run_once(OffsetDateTime::now_utc()).await {
                        tracing::warn!(error = %error, "ViryaOS Autopilot cycle failed");
                    }
                }
            }
        }
    }

    async fn run_once(&self, now: OffsetDateTime) -> Result<(), RepositoryError> {
        // Evaluation, execution and delayed measurement are intentionally isolated.
        // A context-specific query failure must never block already-authorized work
        // or evidence collection from a previous cycle.
        let mut phase_failed = false;

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
                    "ViryaOS recorded first-party growth observations"
                );
            }
            Ok(_) => {}
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS first-party growth metric capture failed");
            }
        }

        let evaluator = EvaluateAutopilot::new(&self.repository, self.workspace_id);
        match evaluator.execute(now).await {
            // A cycle that only started a campaign or only settled a step it
            // will never send has still done something an operator should be
            // able to see. Reading the play counters here is what keeps a
            // recorded omission from being a silent one.
            Ok(report)
                if report.decisions > 0
                    || report.actions_enqueued > 0
                    || report.actions_throttled > 0
                    || report.plays_started > 0
                    || report.play_steps_skipped > 0
                    || report.plays_completed > 0 =>
            {
                tracing::info!(
                    decisions = report.decisions,
                    actions_enqueued = report.actions_enqueued,
                    actions_throttled = report.actions_throttled,
                    plays_started = report.plays_started,
                    play_steps_skipped = report.play_steps_skipped,
                    plays_completed = report.plays_completed,
                    "ViryaOS Autopilot evaluated bounded contexts"
                );
            }
            Ok(_) => {}
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS Autopilot evaluation failed");
            }
        }

        match self
            .repository
            .reconcile_team_handoffs(self.workspace_id, now)
            .await
        {
            Ok(count) if count > 0 => tracing::info!(count, "assigned ViryaOS human handoffs"),
            Ok(_) => {}
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS team handoff reconciliation failed");
            }
        }
        match self
            .repository
            .cancel_unexecutable_actions(self.workspace_id, now)
            .await
        {
            Ok(count) if count > 0 => {
                tracing::info!(count, "cancelled ViryaOS actions with no live executor")
            }
            Ok(_) => {}
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS no-executor sweep failed");
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
                        phase_failed = true;
                        let error_kind = repository_error_kind(error);
                        let retryable = repository_error_retryable(error);
                        tracing::warn!(
                            action_id = %action.id,
                            action_kind = action.payload.action_kind(),
                            error_kind,
                            "ViryaOS Autopilot action failed"
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
                            .await;
                    }
                }
            }
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS Autopilot action claim failed");
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
                for measurement in measurements {
                    let observed_at = OffsetDateTime::now_utc();
                    let result = async {
                        let observed = self
                            .repository
                            .observe_measurement(self.workspace_id, &measurement, observed_at)
                            .await?;
                        let effect = assess_measurement_effect(&measurement, observed)
                            .ok_or(RepositoryError::Unexpected)?;
                        self.repository
                            .complete_measurement(
                                self.workspace_id,
                                &measurement,
                                observed,
                                effect,
                                observed_at,
                            )
                            .await
                    }
                    .await;

                    if let Err(error) = result {
                        phase_failed = true;
                        let error_kind = repository_error_kind(error);
                        let retryable = repository_error_retryable(error);
                        tracing::warn!(
                            measurement_id = %measurement.id,
                            measurement_kind = measurement.kind.as_str(),
                            error_kind,
                            "ViryaOS Autopilot delayed effect measurement failed"
                        );
                        let _ = self
                            .repository
                            .fail_measurement(
                                self.workspace_id,
                                measurement.id,
                                error_kind,
                                retryable,
                                OffsetDateTime::now_utc(),
                            )
                            .await;
                    }
                }
            }
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS Autopilot measurement claim failed");
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
                        phase_failed = true;
                        let error_kind = repository_error_kind(error);
                        let retryable = repository_error_retryable(error);
                        tracing::warn!(
                            play_id = %outcome.play_id,
                            claim = outcome.claim.as_str(),
                            error_kind,
                            "ViryaOS play outcome measurement failed"
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
                            .await;
                    }
                }
            }
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS play outcome claim failed");
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
                        phase_failed = true;
                        let error_kind = repository_error_kind(error);
                        let retryable = repository_error_retryable(error);
                        tracing::warn!(
                            wave_id = %outcome.wave_id,
                            target_kind = outcome.target_kind.as_str(),
                            error_kind,
                            "ViryaOS wave outcome measurement failed"
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
                            .await;
                    }
                }
            }
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS wave outcome claim failed");
            }
        }

        if phase_failed {
            Err(RepositoryError::Unexpected)
        } else {
            Ok(())
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
                        tracing::warn!(error = %error, "ViryaOS team-email dispatch cycle failed");
                    }
                }
            }
        }
    }

    async fn run_once(&self, now: OffsetDateTime) -> Result<(), RepositoryError> {
        let mut phase_failed = false;

        match self
            .repository
            .dispatch_team_handoff_reminders(self.workspace_id, now)
            .await
        {
            Ok(count) if count > 0 => tracing::info!(count, "emitted ViryaOS team reminders"),
            Ok(_) => {}
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS team reminder dispatch failed");
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
                            "ViryaOS team-email action failed"
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
                            .await;
                    }
                }
            }
            Err(error) => {
                phase_failed = true;
                tracing::warn!(error = %error, "ViryaOS team-email action claim failed");
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

const fn repository_error_kind(error: RepositoryError) -> &'static str {
    match error {
        RepositoryError::Unavailable => "repository_unavailable",
        RepositoryError::NotFound => "subject_not_found",
        RepositoryError::Conflict => "state_changed",
        RepositoryError::Unexpected => "unexpected",
    }
}
