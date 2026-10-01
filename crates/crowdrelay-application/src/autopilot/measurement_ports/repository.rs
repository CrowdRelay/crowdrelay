use super::*;
use async_trait::async_trait;

#[async_trait]
pub trait AutopilotMeasurementRepository: Send + Sync {
    async fn claim_due_measurements(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotMeasurement>, RepositoryError>;

    async fn observe_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<f64, RepositoryError>;

    /// Observe dimensioned series without collapsing their units. Scalar-only
    /// repositories retain their existing behavior through this default.
    async fn observe_measurement_with_metrics(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<AutopilotMeasurementObservation, RepositoryError> {
        self.observe_measurement(workspace_id, measurement, now)
            .await
            .map(AutopilotMeasurementObservation::scalar)
    }

    /// Counts the harm events attributable to the measurement's action in
    /// its window — the cost side of the ledger the value observation alone
    /// does not see. Best called before `observe_measurement`: harm exists
    /// whether or not the primary metric is observable, and a cancelled
    /// event's measurement abandons while its harm is still real.
    async fn observe_action_harm(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<HarmObservation, RepositoryError>;

    async fn complete_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observed_value: f64,
        effect: EffectResult,
        harm: Option<&HarmObservation>,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// Persist the exact observed series in the same completion transaction.
    /// A scalar-only implementation must never silently discard a vector.
    async fn complete_measurement_with_metrics(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observation: &AutopilotMeasurementObservation,
        effect: EffectResult,
        harm: Option<&HarmObservation>,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        if !observation.series_lifts.is_empty() {
            return Err(RepositoryError::Unexpected);
        }
        self.complete_measurement(
            workspace_id,
            measurement,
            observation.value,
            effect,
            harm,
            now,
        )
        .await
    }

    /// Fails a measurement, retryable or terminal. `harm` rides along so a
    /// terminal failure merges its `harm:*` keys in the same transaction
    /// that resolves readiness — the evidence row closes with the harm it
    /// observed already on it, and no replay delta can slip between the
    /// two writes. On a retryable miss the row stays `pending` and the
    /// merge is skipped: the retried completion owns it.
    ///
    /// `None` means the observation itself failed — no `harm:*` keys are
    /// written at all. A failed look is not a clean reading: writing zeros
    /// would teach the posterior "no harm" from a measurement that never
    /// looked, and overwrite whatever a sibling attempt already landed.
    async fn fail_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        error_kind: &'static str,
        retryable: bool,
        harm: Option<&HarmObservation>,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}
