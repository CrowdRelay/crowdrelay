//! PostgreSQL adapter for the deterministic ViryaOS Autopilot.

mod actions;
mod actions_execution;
mod control;
mod control_mutations;
mod decisions;
mod deliverability;
mod growth;
mod growth_metrics;
mod measurement;
mod objectives;
mod operations;
mod operator_actions;
mod placements;
mod play_outcomes;
mod plays;
mod runtime;
mod show_cost;
mod state;
mod team;
mod terms;
mod waves;

use std::{collections::HashMap, future::Future, time::Duration};

use async_trait::async_trait;
use crowdrelay_application::{IdempotencyKey, RequestId};
use crowdrelay_application::{
    RepositoryError,
    autopilot::{
        AcquisitionChannels, AutopilotActionPayload, AutopilotActionRepository,
        AutopilotBookingStateRepository, AutopilotContext, AutopilotControlMutation,
        AutopilotControlOverview, AutopilotControlRepository, AutopilotDecisionRepository,
        AutopilotFirstPartyGrowthMetrics, AutopilotGrowthMetricRepository, AutopilotGrowthOverview,
        AutopilotManualStep, AutopilotMarketStateRepository, AutopilotMeasurementKind,
        AutopilotMeasurementRepository, AutopilotMerchStateRepository,
        AutopilotObjectiveRepository, AutopilotPlayLedgerRepository,
        AutopilotPlayOutcomeRepository, AutopilotPolicy, AutopilotPolicyConfig,
        AutopilotPolicySummary, AutopilotRuntimeRepository, AutopilotShowCostRepository,
        AutopilotTicketStateRepository, AutopilotWaveOutcomeRepository, BookingTargetMutation,
        CandidatePersistence, CityMarketSignalMutation, ClaimExecution, ClaimedAutopilotAction,
        ClaimedAutopilotMeasurement, ClaimedPlayOutcome, ClaimedWaveOutcome, DecisionCandidate,
        DeclareGrowthObjective, DeliveryFaultSubject, EvidencePacket, ExecutionClaimMutation,
        ExecutionReportMutation, ExecutorHeartbeatMutation, ExecutorReportStatus,
        FirstPartyGrowthMetricReport, FreezeShowCostPrediction, GROWTH_STALL_AFTER_MINUTES,
        GROWTH_TEMPLATE_KEYS, GrowthCampaignProgress, GrowthDeliveryTotals,
        GrowthMetricPointMutation, GrowthMetricSeriesMutation, GrowthMetricSubject,
        GrowthMetricTrendView, GrowthObjectiveMutation, GrowthObjectiveView, GrowthOutreachSummary,
        GrowthPosture, GrowthPostureView, LiveTermsSnapshot, ManagerBookingPolicySummary,
        ManagerConfigMutation, MerchProductEconomicsMutation, NextBestAction, OutreachKindStanding,
        OutreachWaveAnchor, OutreachWaveSnapshot, OutreachWaveStart, OutreachWaveTransition,
        PLAYLIST_TEMPLATE_KEY, PendingAutopilotAction, PlacementSettlement, PlayAnchor,
        PlayAnchorRef, PlayAudience, PlayClaimView, PlayKindStanding, PlayLedger, PlayLedgerEntry,
        PlayOutcomeObservation, PlayRunSnapshot, PlayStart, PlayStepSettlement,
        PlaylistPlacementSnapshot, PromotionBudgetGuardrailMutation,
        PromotionBudgetGuardrailSummary, PromotionCampaignStateMutation, ProviderActionCorrelation,
        RecentAutopilotAction, RecentAutopilotDecision, RecentAutopilotEffect, RecordDeliveryFault,
        RecordExecutionReport, RecordExecutorHeartbeat, RecordGrowthMetricPoint,
        RecordPlaylistPlacement, RecordRumSample, ReleaseComponentMutation,
        ReleaseComponentSummary, ReleaseLedgerOverview, RumMetricSummary, SetAutopilotAuthority,
        SetGrowthEnvelope, SetGrowthPosture, SetManagerBookingPolicy, SetTourEconomics,
        SettleShowCost, ShowCostLedgerEntry, ShowCostMutation, TeamAssigneeSummary,
        TermsSettlement, TicketAllocationGuardrailMutation, TourEconomicsMutation,
        TourEconomicsSummary, UpsertBookingTarget, UpsertCityMarketSignal,
        UpsertGrowthMetricSeries, UpsertMerchProductEconomics, UpsertPromotionBudgetGuardrail,
        UpsertPromotionCampaignState, UpsertReleaseComponent, UpsertTicketAllocationGuardrail,
        WaveOutcomeObservation,
    },
};
use crowdrelay_brain::GrowthIntelligenceSnapshot;
#[cfg(test)]
use crowdrelay_domain::pricing::TicketYieldPolicy;
use crowdrelay_domain::{
    AutopilotActionId, AutopilotDecisionId, AutopilotMeasurementId, BookingTargetId, CityId,
    EventId, FanId, GrowthMetricSeriesId, MarketSignalId, MerchProductId, MerchVariantId, PlayId,
    PromotionCampaignId, ReleasePlanId, TeamOpportunityId, TicketTypeId, WorkspaceId,
    action_class::ActionClass,
    audience_lifecycle::FanLifecycleSnapshot,
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    beacons::{BeaconCampaignSnapshot, BeaconDiscoverySnapshot, BeaconInviteSnapshot},
    booking::{
        BookingOutreachPhase, BookingReplyDisposition, BookingTargetKind, BookingTargetSnapshot,
        CityOpportunitySnapshot,
    },
    booking_discovery::BookingSupplySnapshot,
    campaign_lifecycle::EventCampaignSnapshot,
    content_supply::ContentSupplySnapshot,
    deliverability::DeliverabilitySnapshot,
    experimentation::ExperimentSnapshot,
    free_reach::{WaveAnchor, WaveSnapshot, WaveState},
    funding::FundingOpportunitySnapshot,
    growth_debt::GrowthDebtObservation,
    growth_envelope::{EnvelopeUsage, GrowthEnvelope},
    growth_metrics::{
        GrowthMetricPolicy, GrowthMetricSnapshot, MetricDirection, MetricPlatform, MetricPoint,
        MetricValueTier, compute_trend, velocity_ratio_basis_points,
    },
    learning::{
        OutcomeRecord, RetirementReason, Standing, StandingPolicy, WaveOutcomeVerdict,
        assess_standing, effective_recipient_ceiling, effective_wave_ceiling,
    },
    live_opportunities::{
        BookingManagerPolicy, LiveOpportunityKind, LiveOpportunityPolicy, LiveOpportunitySnapshot,
        LiveTravelBand,
    },
    market_intelligence::{CityMarketSignal, CityMarketSignalKind, aggregate_city_market_evidence},
    merch_bundle::MerchBundleSnapshot,
    merchandising::{MerchInventorySnapshot, MerchPriceSnapshot},
    negotiation::{TermsLadder, TermsRefusal, TermsSnapshot, TermsState},
    objectives::{GrowthObjective, ObjectivePolicy, ObjectiveScope, assess_objective},
    outreach::{OutreachSnapshot, OutreachTargetKind},
    performance::{EffectAssessment, EffectResult},
    play_measurement::{PlayClaim, PlayOutcomeVerdict},
    playlist_placement::{
        PlacementObservation, PlacementSnapshot, PlacementState, apply_observation,
        suppresses_identity,
    },
    plays::{PlayAnchorKind, PlayKind, PlayPolicy, PlayStepKind, PlayStepState, StepAudience},
    pricing::TicketYieldSnapshot,
    promotion::PromotionPerformanceSnapshot,
    release_autopilot::{ReleaseMilestoneHistory, ReleasePlanSnapshot},
    show_growth::ShowGrowthSnapshot,
    show_operations::ShowTaskSnapshot,
    show_settlement::{
        CostLine, ModelAccuracy, SettlementGap, SettlementPolicy, assess_model_accuracy,
        implied_transport_rate_minor_per_100km,
    },
    target_discovery::OutreachSupplySnapshot,
    tour_economics::{
        CostEvidence, ShowCost, ShowLogistics, TourEconomicsPolicy, TransportBasis, VehicleProfile,
        estimate_show_cost,
    },
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

/// Rebuilds the policy from the row's own columns.
///
/// Reading the row as JSON keeps this one mapping instead of a second `FromRow`
/// struct that would have to be kept in step with the first; every field is
/// still named explicitly, so a renamed column fails loudly rather than reading
/// as a zero.
fn tour_policy_from_columns(columns: &Value) -> TourEconomicsPolicy {
    let default = TourEconomicsPolicy::default();
    let money =
        |key: &str, fallback: i64| columns.get(key).and_then(Value::as_i64).unwrap_or(fallback);
    let small = |key: &str, fallback: u8| {
        columns
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(fallback)
    };
    let large = |key: &str, fallback: u32| {
        columns
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(fallback)
    };
    TourEconomicsPolicy {
        transport_minor_per_100km_round_trip: money(
            "transport_minor_per_100km_round_trip",
            default.transport_minor_per_100km_round_trip,
        ),
        transport_rate_covers_vehicles: small(
            "transport_rate_covers_vehicles",
            default.transport_rate_covers_vehicles,
        ),
        vehicle: VehicleProfile {
            seats: small("vehicle_seats", default.vehicle.seats),
            cargo_litres: large("vehicle_cargo_litres", default.vehicle.cargo_litres),
            fuel_centilitres_per_100km: large(
                "vehicle_fuel_centilitres_per_100km",
                default.vehicle.fuel_centilitres_per_100km,
            ),
        },
        max_vehicles: small("max_vehicles", default.max_vehicles),
        crew_size: small("crew_size", default.crew_size),
        backline_litres: large("backline_litres", default.backline_litres),
        fuel_price_minor_per_litre: money(
            "fuel_price_minor_per_litre",
            default.fuel_price_minor_per_litre,
        ),
        toll_minor_per_km: money("toll_minor_per_km", default.toll_minor_per_km),
        accommodation_minor_per_room_night: money(
            "accommodation_minor_per_room_night",
            default.accommodation_minor_per_room_night,
        ),
        crew_per_room: small("crew_per_room", default.crew_per_room),
        per_diem_minor_per_person_day: money(
            "per_diem_minor_per_person_day",
            default.per_diem_minor_per_person_day,
        ),
        fixed_overhead_minor: money("fixed_overhead_minor", default.fixed_overhead_minor),
        overnight_threshold_km: large("overnight_threshold_km", default.overnight_threshold_km),
        minimum_margin_minor: money("minimum_margin_minor", default.minimum_margin_minor),
    }
}

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

    /// Returns a reference to the underlying connection pool.
    /// Workers that need direct SQL access (e.g. attribution) use this
    /// instead of going through the repository trait methods.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
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
struct TourEconomicsRow {
    transport_minor_per_100km_round_trip: i64,
    transport_rate_covers_vehicles: i16,
    vehicle_seats: i16,
    vehicle_cargo_litres: i32,
    vehicle_fuel_centilitres_per_100km: i32,
    max_vehicles: i16,
    crew_size: i16,
    backline_litres: i32,
    fuel_price_minor_per_litre: i64,
    toll_minor_per_km: i64,
    accommodation_minor_per_room_night: i64,
    crew_per_room: i16,
    per_diem_minor_per_person_day: i64,
    fixed_overhead_minor: i64,
    overnight_threshold_km: i32,
    minimum_margin_minor: i64,
}

#[derive(Debug, FromRow)]
struct GrowthEnvelopeRow {
    agent_enabled: bool,
    dry_run: bool,
    weekly_owned_audience_touches: i32,
    weekly_third_party_touches: i32,
    subject_cooldown_hours: i32,
    max_recipients_per_step: i32,
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
    paid_ticket_count: i64,
    qualified_referrals: i64,
    last_qualified_referral_at: Option<OffsetDateTime>,
    has_referral_code: bool,
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
    editorial_pitch_parked: bool,
    editorial_pitch_done: bool,
    editorial_pitch_escalated_at: Option<OffsetDateTime>,
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
    distance_km: Option<i32>,
    nights_away: Option<i16>,
    committed_shows_year: i64,
    pipeline_shows_year: i64,
    annual_target: i32,
    annual_stretch: i32,
    stretch_minimum_score_basis_points: i32,
    far_shot_minimum_score_basis_points: i32,
    prefer_weekend_one_shots: bool,
    strategic_value_basis_points: i32,
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
include!("autopilot/execution_capabilities.rs");
include!("autopilot/execution_mutations.rs");
include!("autopilot/support.rs");
