//! ViryaOS Autopilot operator control plane.
//!
//! HTTP handlers only validate transport input and delegate to the application
//! control port implemented by PostgreSQL infrastructure. Decision rules remain
//! inside bounded contexts and are never reimplemented here.
use crate::{AppState, IDEMPOTENCY_KEY, Problem, request_id};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    IdempotencyKey, ListCitiesError, RepositoryError, RequestId,
    autopilot::{
        AutopilotBeaconStateRepository, AutopilotBookingDiscoveryRepository,
        AutopilotBookingStateRepository, AutopilotContentStateRepository, AutopilotContext,
        AutopilotControlRepository, AutopilotDecisionRepository,
        AutopilotExperimentStateRepository, AutopilotGrowthMetricRepository,
        AutopilotMarketStateRepository, AutopilotMerchStateRepository,
        AutopilotObjectiveRepository, AutopilotOutreachStateRepository,
        AutopilotPlayLedgerRepository, AutopilotPolicyConfig, AutopilotShowCostRepository,
        AutopilotTargetDiscoveryRepository, AutopilotTeamStateRepository,
        AutopilotTicketStateRepository, CreateExperiment, CreateExperimentVariant,
        DeclareGrowthObjective, DeliveryFaultSubject, ExperimentObservation,
        FreezeShowCostPrediction, GrowthMetricSubject, GrowthMetricTrendView, GrowthObjectiveView,
        GrowthPosture, IngestOutreachCandidate, ManagerConfigSource, OutreachSweepReport,
        PromoterPosition, RecordBeaconReply, RecordBookingReply, RecordDeliveryFault,
        RecordGrowthMetricPoint, RecordOutreachReply, RecordPlaylistPlacement,
        RecordTeamOpportunityProgress, RecordTeamOpportunityTerms, SetAutopilotAuthority,
        SetGrowthEnvelope, SetGrowthPosture, SetManagerBookingPolicy, SetTourEconomics,
        SettleShowCost, ShowCostLedgerEntry, TeamOpportunityKind, TeamOpportunityProgress,
        UpsertBeacon, UpsertBookingTarget, UpsertCityMarketSignal, UpsertContentSource,
        UpsertGrowthMetricSeries, UpsertMerchProductEconomics, UpsertOutreachOpportunity,
        UpsertOutreachTarget, UpsertPromotionBudgetGuardrail, UpsertPromotionCampaignState,
        UpsertReleasePlan, UpsertSubmissionChannel, UpsertTeamOpportunity,
        UpsertTicketAllocationGuardrail, assign_experiment_variant,
    },
};
use crowdrelay_domain::{
    AutopilotActionId, AutopilotDecisionId, BeaconId, BookingTargetId, CityId, CitySlug,
    ContentSourceId, EventId, ExperimentId, ExperimentVariantId, GrowthMetricSeriesId,
    MerchProductId, OutreachOpportunityId, OutreachTargetId, ReleasePlanId, TeamOpportunityId,
    TicketTypeId,
    autonomy::{AutonomyLevel, Confidence},
    beacons::{BeaconKind, BeaconReplyDisposition},
    booking::{BookingReplyDisposition, BookingTargetKind},
    booking_discovery::BookingCandidateInput,
    content_supply::ContentSourceKind,
    deliverability::DeliveryFault,
    experimentation::ExperimentMetric,
    growth_metrics::{
        FeedCoverage, MetricDirection, MetricPlatform, MetricValueTier, off_platform_coverage,
    },
    live_opportunities::{BookingManagerPolicy, LiveTravelBand},
    market_intelligence::CityMarketSignalKind,
    objectives::ObjectiveScope,
    outreach::{OutreachReplyDisposition, OutreachTargetKind},
    playlist_placement::PlacementObservation,
    show_settlement::SettledShowCost,
    target_discovery::{CandidateSource, ChannelCost, RouteKind},
    tour_economics::TourEconomicsPolicy,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
const PRIVATE_NO_STORE: &str = "private, no-store";

mod discovery;
include!("autopilot/requests.rs");
mod runtime;
pub use discovery::discover_team_opportunity;
pub use runtime::{
    execution_claim, execution_report, executor_heartbeat, provider_action, release_component,
    release_ledger, rum,
};

#[derive(Debug, Serialize)]
struct OverviewResponse<T> {
    runtime_enabled: bool,
    #[serde(flatten)]
    overview: T,
}

pub async fn overview(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_control_overview(state.ops.workspace_id())
        .await
    {
        Ok(overview) => private_json(
            StatusCode::OK,
            OverviewResponse {
                runtime_enabled: state.autopilot_runtime_enabled,
                overview,
            },
        ),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Delivery-side growth progress for the Control Plane.
///
/// `overview` reports the action queue; this reports whether the external n8n
/// delivery workers are actually draining the campaigns those actions created.
pub async fn growth(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_growth_overview(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(growth) => private_json(StatusCode::OK, growth),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn chief_of_staff(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_chief_of_staff(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(brief) => private_json(StatusCode::OK, brief),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// One ranked queue across every context.
///
/// `chief-of-staff` answers "what happened"; this answers "what should I do
/// next". The queue is capped by the domain, so this handler has no page size
/// and no filters to get wrong.
pub async fn next_best_actions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_next_best_actions(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(queue) => private_json(StatusCode::OK, queue),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn manager_booking_policy(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_manager_booking_policy(state.ops.workspace_id())
        .await
    {
        Ok(policy) => private_json(StatusCode::OK, policy),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

include!("autopilot/authority_booking.rs");
include!("autopilot/promotion_market.rs");
include!("autopilot/outreach_release.rs");
include!("autopilot/deliverability.rs");
include!("autopilot/experiments_actions.rs");
include!("autopilot/growth_metrics.rs");
include!("autopilot/show_cost.rs");
include!("autopilot/objectives.rs");
include!("autopilot/target_discovery.rs");
include!("autopilot/booking_discovery.rs");
include!("autopilot/scorecard.rs");
include!("autopilot/reply_triage.rs");
include!("autopilot/decision_evidence.rs");
include!("autopilot/cycle.rs");
include!("autopilot/validation.rs");
