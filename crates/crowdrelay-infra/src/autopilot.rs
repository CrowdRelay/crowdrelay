//! PostgreSQL adapter for the deterministic ViryaOS Autopilot.

mod actions;
mod control;
mod decisions;
mod measurement;
mod operations;
mod runtime;
mod state;
mod team;

use std::{collections::HashMap, future::Future, time::Duration};

use async_trait::async_trait;
use crowdrelay_application::{IdempotencyKey, RequestId};
use crowdrelay_application::{
    RepositoryError,
    autopilot::{
        AutopilotActionPayload, AutopilotActionRepository, AutopilotBookingStateRepository,
        AutopilotContext, AutopilotControlMutation, AutopilotControlOverview,
        AutopilotControlRepository, AutopilotDecisionRepository, AutopilotManualStep,
        AutopilotMarketStateRepository, AutopilotMeasurementKind, AutopilotMeasurementRepository,
        AutopilotMerchStateRepository, AutopilotPolicy, AutopilotPolicyConfig,
        AutopilotPolicySummary, AutopilotRuntimeRepository, AutopilotTicketStateRepository,
        BookingTargetMutation, CandidatePersistence, CityMarketSignalMutation, ClaimExecution,
        ClaimedAutopilotAction, ClaimedAutopilotMeasurement, DecisionCandidate,
        ExecutionClaimMutation, ExecutionReportMutation, ExecutorHeartbeatMutation,
        ExecutorReportStatus, ManagerBookingPolicySummary, ManagerConfigMutation,
        MerchProductEconomicsMutation, PendingAutopilotAction, PromotionBudgetGuardrailMutation,
        PromotionBudgetGuardrailSummary, PromotionCampaignStateMutation, ProviderActionCorrelation,
        RecentAutopilotAction, RecentAutopilotDecision, RecentAutopilotEffect,
        RecordExecutionReport, RecordExecutorHeartbeat, RecordRumSample, ReleaseComponentMutation,
        ReleaseComponentSummary, ReleaseLedgerOverview, RumMetricSummary, SetAutopilotAuthority,
        SetManagerBookingPolicy, TeamAssigneeSummary, TicketAllocationGuardrailMutation,
        UpsertBookingTarget, UpsertCityMarketSignal, UpsertMerchProductEconomics,
        UpsertPromotionBudgetGuardrail, UpsertPromotionCampaignState, UpsertReleaseComponent,
        UpsertTicketAllocationGuardrail,
    },
};
use crowdrelay_domain::{
    AutopilotActionId, AutopilotDecisionId, AutopilotMeasurementId, BookingTargetId, CityId,
    EventId, FanId, MarketSignalId, MerchProductId, MerchVariantId, PromotionCampaignId,
    ReleasePlanId, TeamOpportunityId, TicketTypeId, WorkspaceId,
    audience_lifecycle::{FanLifecyclePolicy, FanLifecycleSnapshot},
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    beacons::{BeaconCampaignPolicy, BeaconCampaignSnapshot, BeaconDiscoverySnapshot},
    booking::{
        BookingOpportunityPolicy, BookingOutreachPhase, BookingReplyDisposition, BookingTargetKind,
        BookingTargetSnapshot, CityOpportunitySnapshot,
    },
    campaign_lifecycle::{EventCampaignPolicy, EventCampaignSnapshot},
    content_supply::{ContentSupplyPolicy, ContentSupplySnapshot},
    experimentation::{ExperimentPolicy, ExperimentSnapshot},
    funding::{FundingOpportunitySnapshot, FundingPolicy},
    live_opportunities::{
        BookingManagerPolicy, LiveOpportunityKind, LiveOpportunityPolicy, LiveOpportunitySnapshot,
        LiveTravelBand,
    },
    market_intelligence::{CityMarketSignal, CityMarketSignalKind, aggregate_city_market_evidence},
    merch_bundle::{MerchBundlePolicy, MerchBundleSnapshot},
    merchandising::{
        MerchInventorySnapshot, MerchPricePolicy, MerchPriceSnapshot, MerchReorderPolicy,
    },
    outreach::{OutreachPolicy, OutreachSnapshot},
    performance::{EffectAssessment, EffectResult},
    pricing::{TicketYieldPolicy, TicketYieldSnapshot},
    promotion::{PromotionBudgetPolicy, PromotionPerformanceSnapshot},
    release_autopilot::{ReleaseAutopilotPolicy, ReleaseMilestoneHistory, ReleasePlanSnapshot},
    show_growth::{ShowGrowthPolicy, ShowGrowthSnapshot},
    show_operations::{ShowOperationsPolicy, ShowTaskSnapshot},
};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    config::DatabaseConfig,
    database::{SqlxErrorClass, classify_sqlx_error},
};

const MAX_SNAPSHOTS_PER_CONTEXT: i64 = 500;
const EXTERNAL_ACTION_EVENT_VERSION: i32 = 1;

#[derive(Clone, Debug)]
pub struct PostgresAutopilotRepository {
    pool: PgPool,
    operation_timeout: Duration,
}

impl PostgresAutopilotRepository {
    #[must_use]
    pub fn new(pool: PgPool, database: &DatabaseConfig) -> Self {
        Self {
            pool,
            operation_timeout: database.operation_timeout,
        }
    }

    #[must_use]
    pub fn new_with_timeouts(pool: PgPool, operation_timeout: Duration) -> Self {
        Self {
            pool,
            operation_timeout,
        }
    }

    async fn bounded<T>(
        &self,
        operation: impl Future<Output = Result<T, RepositoryError>>,
    ) -> Result<T, RepositoryError> {
        timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| RepositoryError::Unavailable)?
    }
}

#[derive(Debug, FromRow)]
struct PolicyRow {
    context: String,
    enabled: bool,
    autonomy_level: String,
    minimum_confidence_basis_points: i32,
    max_actions_24h: i32,
    config: Value,
    version: i64,
    guarded_until: Option<OffsetDateTime>,
    guardrail_reason: Option<String>,
}

#[derive(Debug, FromRow)]
struct TicketSnapshotRow {
    ticket_type_id: Uuid,
    current_price_minor: i64,
    paid_quantity: i64,
    capacity: i64,
    sale_capacity: i64,
    paid_last_72h: i64,
    days_to_event: i64,
    last_price_change_at: Option<OffsetDateTime>,
    last_capacity_change_at: Option<OffsetDateTime>,
    allocation_minimum_capacity: Option<i32>,
    allocation_maximum_capacity: Option<i32>,
    allocation_step_capacity: Option<i32>,
    allocation_guardrail_version: Option<i64>,
}

#[derive(Debug, FromRow)]
struct LifecycleSnapshotRow {
    fan_id: Uuid,
    active: bool,
    marketing_consent: bool,
    created_at: OffsetDateTime,
    synesthesia_completed_at: Option<OffsetDateTime>,
    last_marketing_touch_at: Option<OffsetDateTime>,
    has_paid_ticket: bool,
    last_paid_ticket_at: Option<OffsetDateTime>,
    last_event_interest_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct MerchSnapshotRow {
    variant_id: Uuid,
    available_quantity: i64,
    sold_last_30d: i64,
    reorder_in_flight: bool,
    last_reorder_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct MerchPriceSnapshotRow {
    product_id: Uuid,
    current_price_minor: i64,
    minimum_price_minor: i64,
    maximum_price_minor: i64,
    unit_cost_minor: Option<i64>,
    economics_version: i64,
    available_quantity: i64,
    sold_last_7d: i64,
    sold_last_30d: i64,
    last_price_change_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct BookingSnapshotRow {
    city_id: Uuid,
    active_fans: i64,
    new_fans_30d: i64,
    event_interests: i64,
    area_claims: i64,
    months_since_last_show: Option<i64>,
    outreach_in_flight: bool,
    last_outreach_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct BookingTargetRow {
    target_id: Uuid,
    city_id: Uuid,
    target_kind: String,
    display_name: String,
    capacity: Option<i32>,
    version: i64,
    active: bool,
    accepts_booking: bool,
    priority: i32,
    relationship_score: i32,
    outreach_in_flight: bool,
    last_outreach_at: Option<OffsetDateTime>,
    followup_count: i32,
    last_reply_disposition: String,
}

#[derive(Debug, FromRow)]
struct MarketSignalRow {
    city_id: Uuid,
    signal_kind: String,
    score_basis_points: i32,
    confidence_basis_points: i32,
    observed_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct PromotionSnapshotRow {
    campaign_id: Uuid,
    current_daily_budget_minor: i64,
    minimum_daily_budget_minor: i64,
    maximum_daily_budget_minor: i64,
    spend_last_7d_minor: i64,
    attributed_revenue_last_7d_minor: i64,
    workspace_daily_budget_minor: i64,
    workspace_spend_month_to_date_minor: i64,
    workspace_maximum_daily_budget_minor: Option<i64>,
    workspace_maximum_monthly_spend_minor: Option<i64>,
    days_to_event: i64,
    active: bool,
    last_budget_change_at: Option<OffsetDateTime>,
    observed_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct ReleaseSnapshotRow {
    release_id: Uuid,
    title: String,
    release_at: OffsetDateTime,
    active: bool,
    assets_ready: bool,
    communication_enabled: bool,
    press_enabled: bool,
    calendar_seeded: bool,
    announcement_sent: bool,
    press_started: bool,
    fan_warmup_sent: bool,
    countdown_sent: bool,
    release_day_sent: bool,
    sustain_sent: bool,
    wrap_sent: bool,
}

#[derive(Debug, FromRow)]
struct LiveOpportunityRow {
    opportunity_id: Uuid,
    opportunity_kind: String,
    active: bool,
    verified_destination: bool,
    contact_email: Option<String>,
    metadata: Value,
    fit_basis_points: i32,
    reputation_basis_points: i32,
    confidence_basis_points: i32,
    expected_fee_minor: i64,
    estimated_cost_minor: i64,
    application_fee_minor: i64,
    requires_contract: bool,
    exclusive: bool,
    deadline: Option<OffsetDateTime>,
    status: String,
    event_starts_at: Option<OffsetDateTime>,
    travel_band: Option<String>,
    committed_shows_year: i64,
    annual_target: i32,
    annual_stretch: i32,
    stretch_minimum_score_basis_points: i32,
    far_shot_minimum_score_basis_points: i32,
    prefer_weekend_one_shots: bool,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct TeamOpportunityRow {
    opportunity_id: Uuid,
    opportunity_kind: String,
    active: bool,
    verified_destination: bool,
    contact_email: Option<String>,
    metadata: Value,
    fit_basis_points: i32,
    reputation_basis_points: i32,
    confidence_basis_points: i32,
    expected_fee_minor: i64,
    estimated_cost_minor: i64,
    application_fee_minor: i64,
    requires_contract: bool,
    exclusive: bool,
    eligible: bool,
    funding_amount_minor: i64,
    own_contribution_minor: i64,
    deadline: Option<OffsetDateTime>,
    package_status: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct ClaimedActionRow {
    id: Uuid,
    payload: Value,
    attempt_number: i32,
}

#[derive(Debug, FromRow)]
struct ClaimedMeasurementRow {
    id: Uuid,
    action_id: Uuid,
    measurement_kind: String,
    subject_id: Uuid,
    baseline_value: f64,
    action_finished_at: OffsetDateTime,
    attempt_number: i32,
}

#[derive(Debug, FromRow)]
struct PendingActionRow {
    id: Uuid,
    context: String,
    action_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    payload: Value,
    created_at: OffsetDateTime,
    approval_expires_at: Option<OffsetDateTime>,
    assignee_member_id: Option<Uuid>,
    assignee_member_key: Option<String>,
    assignee_display_name: Option<String>,
    assignment_due_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct RecentDecisionRow {
    id: Uuid,
    context: String,
    decision_kind: String,
    confidence_basis_points: i32,
    disposition: String,
    reason: String,
    evaluated_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct RecentActionRow {
    id: Uuid,
    context: String,
    action_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    status: String,
    attempt_count: i32,
    created_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    last_error_kind: Option<String>,
    executor_status: Option<String>,
    executor_id: Option<String>,
    provider_reference: Option<String>,
    executor_reported_at: Option<OffsetDateTime>,
    executor_metadata: Option<Value>,
}

#[derive(Debug, FromRow)]
struct RecentEffectRow {
    measurement_id: Uuid,
    action_id: Uuid,
    context: String,
    measurement_kind: String,
    effect_assessment: String,
    delta_basis_points: i32,
    baseline_value: f64,
    observed_value: f64,
    observed_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct PromotionBudgetGuardrailRow {
    currency: String,
    maximum_total_daily_budget_minor: i64,
    maximum_monthly_spend_minor: i64,
    version: i64,
}

#[derive(Debug, FromRow)]
struct ControlStatsRow {
    queued_actions: i64,
    processing_actions: i64,
    succeeded_24h: i64,
    failed_24h: i64,
    executor_confirmed_24h: i64,
    executor_failed_24h: i64,
    awaiting_executor: i64,
}

#[derive(Debug, FromRow)]
struct ExistingOperatorActionRow {
    id: Uuid,
    action: String,
    target_type: String,
    target_id: Uuid,
    details: Value,
}

// decisions repository implementation lives in `autopilot/decisions.rs`.
// actions repository implementation lives in `autopilot/actions.rs`.
include!("autopilot/mapping.rs");

include!("autopilot/execution.rs");
include!("autopilot/support.rs");
