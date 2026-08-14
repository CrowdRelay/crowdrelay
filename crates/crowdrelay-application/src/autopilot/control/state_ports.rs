/// Administrative port for Autopilot. Keeping this separate from the evaluator
/// port prevents operator/read-model concerns from leaking into decision code.
#[derive(Clone, Debug)]
pub struct UpsertPromotionCampaignState {
    pub provider: String,
    pub external_campaign_key: String,
    pub event_id: Option<EventId>,
    pub currency: String,
    pub current_daily_budget_minor: i64,
    pub minimum_daily_budget_minor: i64,
    pub maximum_daily_budget_minor: i64,
    pub spend_last_7d_minor: i64,
    pub spend_month_to_date_minor: i64,
    pub attributed_revenue_last_7d_minor: i64,
    pub active: bool,
    pub last_budget_change_at: Option<OffsetDateTime>,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct PromotionCampaignStateMutation {
    pub operation_id: uuid::Uuid,
    pub campaign_id: PromotionCampaignId,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertPromotionBudgetGuardrail {
    pub currency: String,
    pub maximum_total_daily_budget_minor: i64,
    pub maximum_monthly_spend_minor: i64,
    /// `0` creates the guardrail; positive values update exactly that version.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PromotionBudgetGuardrailMutation {
    pub operation_id: uuid::Uuid,
    pub currency: String,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertCityMarketSignal {
    pub source: String,
    pub city_id: CityId,
    pub kind: CityMarketSignalKind,
    pub score_basis_points: u16,
    pub confidence: Confidence,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct CityMarketSignalMutation {
    pub operation_id: uuid::Uuid,
    pub signal_id: MarketSignalId,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertBookingTarget {
    pub target_id: Option<BookingTargetId>,
    pub city_id: CityId,
    pub kind: BookingTargetKind,
    pub display_name: String,
    pub contact_email: String,
    /// Optional verified room/event capacity used only for deterministic fit.
    pub capacity: Option<u32>,
    pub priority: u16,
    pub relationship_score: u16,
    pub active: bool,
    pub accepts_booking: bool,
    /// `0` creates a target; positive values update exactly that target version.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BookingTargetMutation {
    pub operation_id: uuid::Uuid,
    pub target_id: BookingTargetId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordBookingReply {
    pub target_id: BookingTargetId,
    pub disposition: BookingReplyDisposition,
    pub occurred_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotBookingStateRepository: Send + Sync {
    async fn upsert_booking_target(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertBookingTarget,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<BookingTargetMutation, RepositoryError>;

    async fn record_booking_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordBookingReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct UpsertBeacon {
    pub beacon_id: Option<BeaconId>,
    pub city_id: Option<CityId>,
    pub kind: BeaconKind,
    pub display_name: String,
    pub contact_email: Option<String>,
    pub destination_url: Option<String>,
    pub source_url: Option<String>,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub do_not_contact: bool,
    pub relationship_score: u16,
    pub relevance_basis_points: u16,
    pub confidence: Confidence,
    pub metadata: serde_json::Value,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BeaconMutation {
    pub operation_id: uuid::Uuid,
    pub beacon_id: BeaconId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordBeaconReply {
    pub beacon_id: BeaconId,
    pub event_id: EventId,
    pub disposition: BeaconReplyDisposition,
    pub occurred_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotBeaconStateRepository: Send + Sync {
    async fn upsert_beacon(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertBeacon,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<BeaconMutation, RepositoryError>;

    async fn record_beacon_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordBeaconReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Copy, Debug)]
pub struct UpsertTicketAllocationGuardrail {
    pub ticket_type_id: crowdrelay_domain::TicketTypeId,
    pub minimum_capacity: u32,
    pub maximum_capacity: u32,
    pub step_capacity: u32,
    /// `0` creates the guardrail row; positive values update exactly that version.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketAllocationGuardrailMutation {
    pub operation_id: uuid::Uuid,
    pub ticket_type_id: crowdrelay_domain::TicketTypeId,
    pub version: i64,
    pub replayed: bool,
}

#[async_trait]
pub trait AutopilotTicketStateRepository: Send + Sync {
    async fn upsert_ticket_allocation_guardrail(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertTicketAllocationGuardrail,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<TicketAllocationGuardrailMutation, RepositoryError>;
}

#[derive(Clone, Copy, Debug)]
pub struct UpsertMerchProductEconomics {
    pub product_id: MerchProductId,
    pub minimum_price_minor: i64,
    pub maximum_price_minor: i64,
    pub unit_cost_minor: Option<i64>,
    /// `0` creates the guardrail row; positive values update exactly that version.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct MerchProductEconomicsMutation {
    pub operation_id: uuid::Uuid,
    pub product_id: MerchProductId,
    pub version: i64,
    pub replayed: bool,
}

#[async_trait]
pub trait AutopilotMerchStateRepository: Send + Sync {
    async fn upsert_merch_product_economics(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertMerchProductEconomics,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<MerchProductEconomicsMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct UpsertOutreachTarget {
    pub target_id: Option<OutreachTargetId>,
    pub kind: OutreachTargetKind,
    pub display_name: String,
    pub contact_email: String,
    pub priority: u16,
    pub relationship_score: u16,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub do_not_contact: bool,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutreachTargetMutation {
    pub operation_id: uuid::Uuid,
    pub target_id: OutreachTargetId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertOutreachOpportunity {
    pub opportunity_id: Option<OutreachOpportunityId>,
    pub target_id: OutreachTargetId,
    pub source: String,
    pub subject_kind: String,
    pub subject_key: String,
    pub template_key: String,
    pub relevance_basis_points: u16,
    pub confidence: Confidence,
    pub active: bool,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutreachOpportunityMutation {
    pub operation_id: uuid::Uuid,
    pub opportunity_id: OutreachOpportunityId,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordOutreachReply {
    pub target_id: OutreachTargetId,
    pub opportunity_id: Option<OutreachOpportunityId>,
    pub disposition: OutreachReplyDisposition,
    pub occurred_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotOutreachStateRepository: Send + Sync {
    async fn upsert_outreach_target(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertOutreachTarget,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachTargetMutation, RepositoryError>;
    async fn upsert_outreach_opportunity(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertOutreachOpportunity,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachOpportunityMutation, RepositoryError>;
    async fn record_outreach_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordOutreachReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct UpsertReleasePlan {
    pub release_id: Option<ReleasePlanId>,
    pub source_key: String,
    pub title: String,
    pub release_at: OffsetDateTime,
    pub listen_url: Option<String>,
    pub active: bool,
    pub assets_ready: bool,
    pub communication_enabled: bool,
    pub press_enabled: bool,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReleasePlanMutation {
    pub operation_id: uuid::Uuid,
    pub release_id: ReleasePlanId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamOpportunityKind {
    Festival,
    Showcase,
    ReviewContest,
    SupportSlot,
    Funding,
}

impl TeamOpportunityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Festival => "festival",
            Self::Showcase => "showcase",
            Self::ReviewContest => "review_contest",
            Self::SupportSlot => "support_slot",
            Self::Funding => "funding",
        }
    }
}

#[derive(Clone, Debug)]
pub struct UpsertTeamOpportunity {
    pub opportunity_id: Option<TeamOpportunityId>,
    pub kind: TeamOpportunityKind,
    pub source: String,
    pub external_key: String,
    pub title: String,
    pub organization: String,
    pub destination_url: Option<String>,
    pub contact_email: Option<String>,
    pub verified_destination: bool,
    pub fit_basis_points: u16,
    pub reputation_basis_points: u16,
    pub confidence: Confidence,
    pub currency: String,
    pub expected_fee_minor: i64,
    pub estimated_cost_minor: i64,
    pub application_fee_minor: i64,
    pub requires_contract: bool,
    pub exclusive: bool,
    pub eligible: bool,
    pub funding_amount_minor: i64,
    pub own_contribution_minor: i64,
    pub deadline: Option<OffsetDateTime>,
    pub event_starts_at: Option<OffsetDateTime>,
    pub country_code: Option<String>,
    pub travel_band: Option<LiveTravelBand>,
    pub metadata: serde_json::Value,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TeamOpportunityMutation {
    pub operation_id: uuid::Uuid,
    pub opportunity_id: TeamOpportunityId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamOpportunityProgress {
    PackageReady,
    Submitted,
    Replied,
    Won,
    Lost,
    Dismissed,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordTeamOpportunityProgress {
    pub opportunity_id: TeamOpportunityId,
    pub progress: TeamOpportunityProgress,
    pub occurred_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotTeamStateRepository: Send + Sync {
    async fn upsert_release_plan(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertReleasePlan,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ReleasePlanMutation, RepositoryError>;
    async fn upsert_team_opportunity(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertTeamOpportunity,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<TeamOpportunityMutation, RepositoryError>;
    async fn record_team_opportunity_progress(
        &self,
        workspace_id: WorkspaceId,
        command: RecordTeamOpportunityProgress,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct UpsertContentSource {
    pub source_id: Option<ContentSourceId>,
    pub kind: ContentSourceKind,
    pub source_key: String,
    pub title: String,
    pub occurred_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub metadata: serde_json::Value,
    pub expected_version: i64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ContentSourceMutation {
    pub operation_id: uuid::Uuid,
    pub source_id: ContentSourceId,
    pub version: i64,
    pub replayed: bool,
}
#[async_trait]
pub trait AutopilotContentStateRepository: Send + Sync {
    async fn upsert_content_source(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertContentSource,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ContentSourceMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct CreateExperimentVariant {
    pub key: String,
    pub allocation_basis_points: u16,
}
#[derive(Clone, Debug)]
pub struct CreateExperiment {
    pub slug: String,
    pub metric: ExperimentMetric,
    pub variants: Vec<CreateExperimentVariant>,
    pub start: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentMutation {
    pub operation_id: uuid::Uuid,
    pub experiment_id: ExperimentId,
    pub replayed: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct ExperimentObservation {
    pub experiment_id: ExperimentId,
    pub variant_id: ExperimentVariantId,
    pub exposures_delta: u32,
    pub conversions_delta: u32,
    pub value_minor_delta: i64,
    pub observed_at: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub struct ExperimentAssignmentVariant {
    pub slot: ExperimentAllocationSlot,
    pub key: String,
}

#[derive(Clone, Debug)]
pub struct ExperimentAssignmentSource {
    pub experiment_id: ExperimentId,
    pub version: i64,
    pub variants: Vec<ExperimentAssignmentVariant>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExperimentAssignment {
    pub experiment_id: ExperimentId,
    pub experiment_version: i64,
    pub variant_id: ExperimentVariantId,
    pub variant_key: String,
}

#[async_trait]
pub trait AutopilotExperimentStateRepository: Send + Sync {
    async fn create_experiment(
        &self,
        workspace_id: WorkspaceId,
        command: CreateExperiment,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ExperimentMutation, RepositoryError>;

    async fn record_experiment_observation(
        &self,
        workspace_id: WorkspaceId,
        command: ExperimentObservation,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    async fn load_experiment_assignment(
        &self,
        workspace_id: WorkspaceId,
        experiment_id: ExperimentId,
    ) -> Result<ExperimentAssignmentSource, RepositoryError>;
}

pub async fn assign_experiment_variant<R: AutopilotExperimentStateRepository>(
    repository: &R,
    workspace_id: WorkspaceId,
    experiment_id: ExperimentId,
    assignment_key: &str,
) -> Result<ExperimentAssignment, RepositoryError> {
    let normalized_key = assignment_key.trim();
    if normalized_key.is_empty() || normalized_key.len() > 200 {
        return Err(RepositoryError::Unexpected);
    }

    let source = repository
        .load_experiment_assignment(workspace_id, experiment_id)
        .await?;
    let slots = source
        .variants
        .iter()
        .map(|variant| variant.slot)
        .collect::<Vec<_>>();
    let selected = assign_variant(experiment_id, normalized_key.as_bytes(), &slots)
        .ok_or(RepositoryError::Conflict)?;
    let variant = source
        .variants
        .into_iter()
        .find(|variant| variant.slot.variant_id == selected)
        .ok_or(RepositoryError::Unexpected)?;

    Ok(ExperimentAssignment {
        experiment_id: source.experiment_id,
        experiment_version: source.version,
        variant_id: selected,
        variant_key: variant.key,
    })
}

#[async_trait]
pub trait AutopilotMarketStateRepository: Send + Sync {
    async fn upsert_promotion_budget_guardrail(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertPromotionBudgetGuardrail,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<PromotionBudgetGuardrailMutation, RepositoryError>;

    async fn upsert_promotion_campaign_state(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertPromotionCampaignState,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<PromotionCampaignStateMutation, RepositoryError>;

    async fn upsert_city_market_signal(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertCityMarketSignal,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<CityMarketSignalMutation, RepositoryError>;
}
