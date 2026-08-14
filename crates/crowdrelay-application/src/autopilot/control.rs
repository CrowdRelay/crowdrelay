//! Operator control plane and provider-neutral state ingress ports.

use async_trait::async_trait;
use crowdrelay_domain::{
    AutopilotActionId, AutopilotMeasurementId, BeaconId, BookingTargetId, CityId, ContentSourceId,
    EventId, ExperimentId, ExperimentVariantId, MarketSignalId, MerchProductId,
    OutreachOpportunityId, OutreachTargetId, PromotionCampaignId, ReleasePlanId, TeamOpportunityId,
    WorkspaceId,
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    beacons::{BeaconKind, BeaconReplyDisposition},
    booking::{BookingReplyDisposition, BookingTargetKind},
    content_supply::ContentSourceKind,
    experimentation::{ExperimentAllocationSlot, ExperimentMetric, assign_variant},
    live_opportunities::{BookingManagerPolicy, LiveTravelBand},
    market_intelligence::CityMarketSignalKind,
    outreach::{OutreachReplyDisposition, OutreachTargetKind},
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::{
    model::{AutopilotActionPayload, AutopilotContext},
    ports::AutopilotMeasurementKind,
};
use crate::{IdempotencyKey, RepositoryError, RequestId};

#[derive(Clone, Debug, Serialize)]
pub struct AutopilotPolicySummary {
    pub context: AutopilotContext,
    pub enabled: bool,
    pub autonomy_level: AutonomyLevel,
    pub minimum_confidence: Confidence,
    pub max_actions_24h: u32,
    pub version: i64,
    pub guarded_until: Option<OffsetDateTime>,
    pub guardrail_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PromotionBudgetGuardrailSummary {
    pub currency: String,
    pub maximum_total_daily_budget_minor: i64,
    pub maximum_monthly_spend_minor: i64,
    pub version: i64,
}

/// Non-sensitive owner shown consistently across staff surfaces. Contact email
/// stays private in the notification executor payload.
#[derive(Clone, Debug, Serialize)]
pub struct TeamAssigneeSummary {
    pub member_id: uuid::Uuid,
    pub member_key: String,
    pub display_name: String,
}

/// Human-actionable Autopilot job waiting in the approval queue.
#[derive(Clone, Debug, Serialize)]
pub struct PendingAutopilotAction {
    pub id: AutopilotActionId,
    pub context: AutopilotContext,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub payload: AutopilotActionPayload,
    pub created_at: OffsetDateTime,
    pub approval_expires_at: Option<OffsetDateTime>,
    pub assignee: Option<TeamAssigneeSummary>,
    pub assignment_due_at: Option<OffsetDateTime>,
}

/// Compact immutable decision trail shown in the operator cockpit.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotDecision {
    pub id: crowdrelay_domain::AutopilotDecisionId,
    pub context: AutopilotContext,
    pub decision_kind: String,
    pub confidence: Confidence,
    pub disposition: PolicyDisposition,
    pub reason: String,
    pub evaluated_at: OffsetDateTime,
}

/// A bounded human-only step returned by an executor when a free distribution
/// surface cannot be automated safely (for example CAPTCHA/login verification).
/// Only public destination data is exposed to staff surfaces; arbitrary executor
/// metadata remains private to the execution ledger.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotManualStep {
    pub destination: String,
    pub url: String,
    pub what_to_do: String,
    pub why_it_matters: String,
}

/// Recent action execution evidence. This is deliberately compact and contains
/// no recipient PII; the cockpit is an exception surface, not a data browser.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotAction {
    pub id: AutopilotActionId,
    pub context: AutopilotContext,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub status: String,
    pub attempt_count: u32,
    pub created_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
    pub last_error_kind: Option<String>,
    pub executor_status: Option<String>,
    pub executor_id: Option<String>,
    pub provider_reference: Option<String>,
    pub executor_reported_at: Option<OffsetDateTime>,
    pub manual_steps: Vec<AutopilotManualStep>,
}

/// A delayed, measured effect of one successfully executed Autopilot action.
/// This is evidence for calibration, not proof of causality; the metric name is
/// intentionally explicit about proxies where exact attribution is unavailable.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotEffect {
    pub measurement_id: AutopilotMeasurementId,
    pub action_id: AutopilotActionId,
    pub context: AutopilotContext,
    pub measurement_kind: AutopilotMeasurementKind,
    pub assessment: crowdrelay_domain::performance::EffectAssessment,
    pub delta_basis_points: i32,
    pub baseline_value: f64,
    pub observed_value: f64,
    pub observed_at: OffsetDateTime,
}

/// Exception-first operator view. The UI should emphasize `needs_you`; recent
/// decisions/actions and measured effects exist as evidence, not as another dashboard
/// operators must watch.
#[derive(Clone, Debug, Serialize)]
pub struct RumMetricSummary {
    pub surface: String,
    pub metric_key: String,
    pub samples_24h: i64,
    pub p75: f64,
    pub p95: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutopilotControlOverview {
    pub policies: Vec<AutopilotPolicySummary>,
    pub promotion_budget_guardrails: Vec<PromotionBudgetGuardrailSummary>,
    pub needs_you: Vec<PendingAutopilotAction>,
    /// Active human owners eligible for an explicit operator re-assignment.
    /// No contact PII is exposed on staff read models.
    pub available_assignees: Vec<TeamAssigneeSummary>,
    pub recent_decisions: Vec<RecentAutopilotDecision>,
    pub recent_actions: Vec<RecentAutopilotAction>,
    pub recent_effects: Vec<RecentAutopilotEffect>,
    pub queued_actions: i64,
    pub processing_actions: i64,
    pub succeeded_24h: i64,
    pub failed_24h: i64,
    pub executor_confirmed_24h: i64,
    pub executor_failed_24h: i64,
    pub awaiting_executor: i64,
    pub release_ledger: ReleaseLedgerOverview,
    pub rum_metrics_24h: Vec<RumMetricSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffOpportunity {
    pub context: AutopilotContext,
    pub decision_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub confidence: Confidence,
    pub reason: String,
    pub needs_approval: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffShowTask {
    pub event_id: EventId,
    pub event_title: String,
    pub task_key: String,
    pub status: String,
    pub starts_at: OffsetDateTime,
}

/// Time-sensitive fact surfaced by the Chief-of-Staff read model. This does
/// not create another task system: approvals and team-opportunity deadlines
/// remain owned by their existing bounded contexts.
#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffAttentionItem {
    pub kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub title: String,
    pub detail: String,
    pub due_at: OffsetDateTime,
    pub urgency: String,
}

/// Deterministic daily operating brief. `estimated_minutes_saved_24h` is a
/// coarse action-kind workload estimate, not fabricated AI precision.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotChiefOfStaff {
    pub executed_24h: i64,
    pub failed_24h: i64,
    pub needs_you: i64,
    pub estimated_minutes_saved_24h: i64,
    pub measured_improved_7d: i64,
    pub measured_neutral_7d: i64,
    pub measured_worsened_7d: i64,
    pub emitted_24h: i64,
    pub executor_confirmed_24h: i64,
    pub executor_failed_24h: i64,
    pub attention_items: Vec<ChiefOfStaffAttentionItem>,
    pub top_opportunities: Vec<ChiefOfStaffOpportunity>,
    pub show_tasks: Vec<ChiefOfStaffShowTask>,
}

/// Authority-only policy mutation. Domain-specific thresholds remain typed
/// config owned by code/migrations until a dedicated validated editor exists.
#[derive(Clone, Copy, Debug)]
pub struct SetAutopilotAuthority {
    pub context: AutopilotContext,
    pub enabled: bool,
    pub autonomy_level: AutonomyLevel,
    pub minimum_confidence: Confidence,
    pub max_actions_24h: u32,
    /// Optimistic-concurrency guard from the overview read model.
    pub expected_version: i64,
}

/// Result of an audited operator mutation.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotControlMutation {
    pub operation_id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub status: String,
    pub replayed: bool,
}

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
    pub synced_at: Option<OffsetDateTime>,
}

#[async_trait]
pub trait AutopilotControlRepository: Send + Sync {
    async fn load_control_overview(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<AutopilotControlOverview, RepositoryError>;

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
    pub workflow_attested_at: Option<OffsetDateTime>,
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
