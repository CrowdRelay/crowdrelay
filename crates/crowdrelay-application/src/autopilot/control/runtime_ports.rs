#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerConfigSource {
    GoogleSheets,
    Operator,
}

impl ManagerConfigSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GoogleSheets => "google_sheets",
            Self::Operator => "operator",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SetManagerBookingPolicy {
    pub policy: BookingManagerPolicy,
    pub source: ManagerConfigSource,
    pub source_revision: Option<String>,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagerConfigMutation {
    pub operation_id: uuid::Uuid,
    pub config_key: String,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagerBookingPolicySummary {
    pub policy: BookingManagerPolicy,
    pub source: String,
    pub source_revision: Option<String>,
    pub version: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub synced_at: Option<OffsetDateTime>,
}

#[async_trait]
pub trait AutopilotControlRepository: Send + Sync {
    async fn load_control_overview(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<AutopilotControlOverview, RepositoryError>;

    /// Delivery-side progress for the growth loop. Separate from the control
    /// overview because it reads the campaign delivery ledger rather than the
    /// action queue, and operators need it even when no action is pending.
    async fn load_growth_overview(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<AutopilotGrowthOverview, RepositoryError>;

    async fn load_chief_of_staff(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<AutopilotChiefOfStaff, RepositoryError>;

    async fn load_manager_booking_policy(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<ManagerBookingPolicySummary, RepositoryError>;

    async fn set_manager_booking_policy(
        &self,
        workspace_id: WorkspaceId,
        command: SetManagerBookingPolicy,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ManagerConfigMutation, RepositoryError>;

    async fn set_authority(
        &self,
        workspace_id: WorkspaceId,
        command: SetAutopilotAuthority,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    async fn assign_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        member_key: &str,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    async fn approve_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    async fn cancel_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorReportStatus {
    Accepted,
    Executing,
    Succeeded,
    Failed,
}

impl ExecutorReportStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Executing => "executing",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecordExecutionReport {
    pub action_id: AutopilotActionId,
    pub receipt_key: String,
    pub executor_id: String,
    pub status: ExecutorReportStatus,
    pub claim_token: Option<uuid::Uuid>,
    pub provider_reference: Option<String>,
    pub error_kind: Option<String>,
    pub metadata: serde_json::Value,
    pub occurred_at: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub struct ClaimExecution {
    pub action_id: AutopilotActionId,
    pub executor_id: String,
    pub occurred_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionClaimMutation {
    pub action_id: AutopilotActionId,
    pub executor_id: String,
    pub disposition: String,
    pub claim_token: Option<uuid::Uuid>,
    pub attempt_number: u32,
    pub provider_reference: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionReportMutation {
    pub report_id: uuid::Uuid,
    pub action_id: AutopilotActionId,
    pub status: ExecutorReportStatus,
    pub replayed: bool,
}

/// Durable provider correlation resolved from the immutable execution-receipt ledger.
/// External adapters use this to map provider-native identifiers (for example a
/// Gmail thread ID) back to the CrowdRelay-owned action without keeping business
/// state in n8n.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderActionCorrelation {
    pub action_id: AutopilotActionId,
    pub context: AutopilotContext,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub executor_id: String,
    pub provider_reference: String,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExecutorCapability {
    pub capability: String,
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct RecordExecutorHeartbeat {
    pub executor_id: String,
    pub version: String,
    pub manifest_sha: String,
    pub capabilities: Vec<ExecutorCapability>,
    pub metadata: serde_json::Value,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutorHeartbeatMutation {
    pub executor_id: String,
    pub capability_count: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub struct UpsertReleaseComponent {
    pub component_key: String,
    pub environment: String,
    pub source_sha: String,
    pub artifact_digest: Option<String>,
    pub deploy_ref: Option<String>,
    pub version: Option<String>,
    pub manifest_sha: Option<String>,
    pub metadata: serde_json::Value,
    pub observed_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReleaseComponentSummary {
    pub component_key: String,
    pub environment: String,
    pub source_sha: String,
    pub artifact_digest: Option<String>,
    pub deploy_ref: Option<String>,
    pub version: Option<String>,
    pub manifest_sha: Option<String>,
    /// SHA-256 of the dependency lockfile used for the deployed build.
    pub dependency_lock_sha256: Option<String>,
    /// SHA-256 of the build artifact manifest when the component has one.
    pub artifact_manifest_sha256: Option<String>,
    /// Public SHA-256 of the secretless n8n workflow attestation. Only the n8n
    /// component populates these fields; private workflow JSON never enters the
    /// release ledger read model.
    pub workflow_attestation_sha: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub workflow_attested_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    pub stale: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReleaseLedgerOverview {
    pub components: Vec<ReleaseComponentSummary>,
    pub missing_components: Vec<String>,
    pub backend_sha_drift: bool,
    pub executor_manifest_drift: bool,
    pub active_executor_count: i64,
    pub guarded_executor_count: i64,
    pub active_executor_manifest_shas: Vec<String>,
    /// Number of currently healthy executors advertising the team-email
    /// provider capability. This is stronger than a desired-state manifest bit.
    pub active_team_email_executor_count: i64,
    /// True only when the current n8n release component carries a fresh
    /// attestation explicitly bound to the same route-manifest SHA.
    pub n8n_attestation_ready: bool,
    /// Operator-level truth: desired route + attested matching manifest + live
    /// non-guarded executor capability.
    pub team_email_live: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReleaseComponentMutation {
    pub component_key: String,
    pub environment: String,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub struct RecordRumSample {
    pub surface: String,
    pub metric_key: String,
    pub value: f64,
    pub route: Option<String>,
    pub device_class: Option<String>,
    pub release: Option<String>,
    pub metadata: serde_json::Value,
    pub observed_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotRuntimeRepository: Send + Sync {
    async fn claim_execution(
        &self,
        workspace_id: WorkspaceId,
        command: ClaimExecution,
    ) -> Result<ExecutionClaimMutation, RepositoryError>;

    async fn record_execution_report(
        &self,
        workspace_id: WorkspaceId,
        command: RecordExecutionReport,
    ) -> Result<ExecutionReportMutation, RepositoryError>;

    async fn find_provider_action(
        &self,
        workspace_id: WorkspaceId,
        executor_id: &str,
        provider_reference: &str,
    ) -> Result<Option<ProviderActionCorrelation>, RepositoryError>;

    async fn record_executor_heartbeat(
        &self,
        workspace_id: WorkspaceId,
        command: RecordExecutorHeartbeat,
    ) -> Result<ExecutorHeartbeatMutation, RepositoryError>;

    async fn upsert_release_component(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertReleaseComponent,
    ) -> Result<ReleaseComponentMutation, RepositoryError>;

    async fn load_release_ledger(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<ReleaseLedgerOverview, RepositoryError>;

    async fn record_rum_sample(
        &self,
        workspace_id: WorkspaceId,
        command: RecordRumSample,
    ) -> Result<(), RepositoryError>;

    async fn load_rum_summaries(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<RumMetricSummary>, RepositoryError>;
}
