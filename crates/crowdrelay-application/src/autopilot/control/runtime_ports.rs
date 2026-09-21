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

    /// The content page's own queue slice — pending content actions plus the
    /// material count and the titles they cite — without the overview's
    /// cockpit-wide fan-out.
    async fn load_content_pipeline(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<ContentPipeline, RepositoryError>;

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

    /// One ranked queue across every context, already capped by the domain.
    ///
    /// Separate from `load_chief_of_staff` because the brief answers "what
    /// happened" and this answers "what should I do next" — they are read at
    /// different moments and the queue must stay short enough to work through.
    async fn load_next_best_actions(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<NextBestAction>, RepositoryError>;

    /// Every community relay batch — a content piece's whole community spread
    /// read as one campaign: the content, the image, the target list, the
    /// cadence, and once approved the posts and clicks the spread produced.
    ///
    /// Separate from the action queue because the batch *is* the queue entry —
    /// the per-community deliveries it covers are detail rows under it, not
    /// cards of their own.
    async fn load_community_relays(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<CommunityRelayBatchView>, RepositoryError>;

    /// The scout shortlist: every tracked opportunity with its link, costed
    /// figures, staleness and latest decision — including the rows that are
    /// closed, ineligible or stale, because a review surface that hides its
    /// rejections makes the operator re-check them by hand.
    async fn load_opportunity_shortlist(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<OpportunityShortlist, RepositoryError>;

    /// Which channels produced people who stayed.
    ///
    /// The question a zero-budget campaign lives on, and the one the system
    /// could not answer at all before channel identity existed on links.
    async fn load_acquisition_channels(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<AcquisitionChannels, RepositoryError>;

    /// The band's vehicles and rates. Read before editing so an operator sees
    /// the version they are about to overwrite.
    async fn load_tour_economics_config(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<TourEconomicsSummary, RepositoryError>;

    /// Which posture is live. `None` posture means never applied: every
    /// surface still holds its provisioned defaults.
    async fn load_growth_posture(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GrowthPostureView, RepositoryError>;

    /// Applies a posture atomically: every context level, all four class
    /// ceilings and the envelope switches, one transaction, one ledger entry.
    async fn set_growth_posture(
        &self,
        workspace_id: WorkspaceId,
        command: SetGrowthPosture,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    async fn set_tour_economics(
        &self,
        workspace_id: WorkspaceId,
        command: SetTourEconomics,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<TourEconomicsMutation, RepositoryError>;

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

    /// Sets the envelope, kill switch included, under optimistic concurrency.
    ///
    /// Stopping the agent must never depend on anything the agent itself runs,
    /// so this is a direct write with no queue and no worker in the path.
    async fn set_growth_envelope(
        &self,
        workspace_id: WorkspaceId,
        command: SetGrowthEnvelope,
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

    /// Approves a parked action, optionally with an operator revision.
    ///
    /// `revision` maps draft fields to the operator's replacement text. The
    /// domain rules in `draft_revision` decide what may change — words a
    /// human reads, never recipients, costs, or routing — and a refused
    /// revision refuses the approval rather than approving the original.
    async fn approve_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
        revision: Option<&std::collections::BTreeMap<String, String>>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    async fn cancel_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Records that a human handled this finding outside the system.
    ///
    /// A first-class outcome rather than a dismissal: an opportunity a human
    /// took is a success, and recording it as ignored would teach the ranker
    /// the wrong thing. The decision leaves every read model, and any action
    /// of it still parked is withdrawn so it cannot later go out anyway.
    async fn mark_decision_handled_externally(
        &self,
        workspace_id: WorkspaceId,
        decision_id: AutopilotDecisionId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Approves a whole free-reach wave at once.
    ///
    /// The point of a wave: forty individual approvals is how a human stops
    /// approving. Only a sealed wave may be approved — one still being drafted
    /// would grow after somebody read it.
    async fn approve_outreach_wave(
        &self,
        workspace_id: WorkspaceId,
        wave_id: uuid::Uuid,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Approves a synced post's whole relay ladder at once (P.5).
    ///
    /// One "yes" over every rung the post already asked for — the owned-audience
    /// push and one community relay per admitted community. A rung decided
    /// later (a community admitted after the approval) asks on its own: the
    /// ladder is the spread the operator could read, not a standing yes to
    /// whatever the post might still become.
    async fn approve_relay_ladder(
        &self,
        workspace_id: WorkspaceId,
        source_id: uuid::Uuid,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Approves a show's whole growth ladder at once (P.4).
    ///
    /// Releases the rungs already parked in `awaiting_approval` for the event
    /// and pre-authorizes the ones not yet decided: while the approval row is
    /// live, a lever whose only remaining gate is the human one auto-executes
    /// on schedule. The domain's own evidence gates still apply — a denied
    /// rung stays denied.
    async fn approve_show_ladder(
        &self,
        workspace_id: WorkspaceId,
        event_id: EventId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Cancels the queued rungs a relay-ladder approval released.
    ///
    /// "Stop the rest of this post's spread" — rungs already running or
    /// finished keep their record, and a rung a person approved on its own
    /// keeps its approval. With no ladder row to close, a second revoke simply
    /// finds nothing left to cancel.
    async fn revoke_relay_ladder(
        &self,
        workspace_id: WorkspaceId,
        source_id: uuid::Uuid,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Withdraws a show's ladder approval.
    ///
    /// Future rungs go back to asking individually, and rungs the ladder
    /// released but that have not executed yet are cancelled — the operator
    /// who revokes means "stop the remaining ladder". A rung approved on its
    /// own, outside the ladder, keeps its approval.
    async fn revoke_show_ladder(
        &self,
        workspace_id: WorkspaceId,
        event_id: EventId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Approves a community relay batch — one "yes" over every delivery a
    /// piece of content drafted to a community.
    ///
    /// Unlike the relay ladder this is a standing answer, not a point-in-time
    /// release: the batch row persists, so a draft that lands after the
    /// approval queues under it instead of parking a second ask. Deliveries
    /// drip out at `interval_seconds` on the community executor; the card the
    /// operator approved said exactly that cadence.
    async fn approve_community_relay(
        &self,
        workspace_id: WorkspaceId,
        source_id: uuid::Uuid,
        interval_seconds: Option<i32>,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Revokes a community relay batch — "stop the rest of this spread".
    ///
    /// Parked deliveries lose their ask, queued deliveries are cancelled,
    /// and the community_posts rows still waiting in the drip are cancelled.
    /// A post already on Reddit keeps its record; a revoke cannot unpost.
    async fn revoke_community_relay(
        &self,
        workspace_id: WorkspaceId,
        source_id: uuid::Uuid,
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
