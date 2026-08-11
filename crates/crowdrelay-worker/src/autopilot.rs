//! Background evaluator/executor for deterministic ViryaOS Autopilot actions.

use std::time::Duration;

use crowdrelay_application::{
    RepositoryError,
    autopilot::{
        AutopilotActionRepository, AutopilotMeasurementRepository, EvaluateAutopilot,
        assess_measurement_effect,
    },
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};

const ACTION_BATCH_SIZE: u32 = 32;
const MEASUREMENT_BATCH_SIZE: u32 = 16;

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

        let evaluator = EvaluateAutopilot::new(&self.repository, self.workspace_id);
        match evaluator.execute(now).await {
            Ok(report)
                if report.decisions > 0
                    || report.actions_enqueued > 0
                    || report.actions_throttled > 0 =>
            {
                tracing::info!(
                    decisions = report.decisions,
                    actions_enqueued = report.actions_enqueued,
                    actions_throttled = report.actions_throttled,
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
            .claim_due_actions(self.workspace_id, ACTION_BATCH_SIZE, now)
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
