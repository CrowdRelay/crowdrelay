//! ViryaOS Autopilot operator control plane.
//!
//! HTTP handlers only validate transport input and delegate to the application
//! control port implemented by PostgreSQL infrastructure. Decision rules remain
//! inside bounded contexts and are never reimplemented here.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    IdempotencyKey, RepositoryError, RequestId,
    autopilot::{
        AutopilotBookingStateRepository, AutopilotContentStateRepository, AutopilotContext,
        AutopilotControlRepository, AutopilotExperimentStateRepository,
        AutopilotMarketStateRepository, AutopilotMerchStateRepository,
        AutopilotOutreachStateRepository, AutopilotTeamStateRepository,
        AutopilotTicketStateRepository, CreateExperiment, CreateExperimentVariant,
        ExperimentObservation, RecordBookingReply, RecordOutreachReply,
        RecordTeamOpportunityProgress, SetAutopilotAuthority, TeamOpportunityKind,
        TeamOpportunityProgress, UpsertBookingTarget, UpsertCityMarketSignal, UpsertContentSource,
        UpsertMerchProductEconomics, UpsertOutreachOpportunity, UpsertOutreachTarget,
        UpsertPromotionBudgetGuardrail, UpsertPromotionCampaignState, UpsertReleasePlan,
        UpsertTeamOpportunity, UpsertTicketAllocationGuardrail, assign_experiment_variant,
    },
};
use crowdrelay_domain::{
    AutopilotActionId, BookingTargetId, CityId, ContentSourceId, EventId, ExperimentId,
    ExperimentVariantId, MerchProductId, OutreachOpportunityId, OutreachTargetId, ReleasePlanId,
    TeamOpportunityId, TicketTypeId,
    autonomy::{AutonomyLevel, Confidence},
    booking::{BookingReplyDisposition, BookingTargetKind},
    content_supply::ContentSourceKind,
    experimentation::ExperimentMetric,
    market_intelligence::CityMarketSignalKind,
    outreach::{OutreachReplyDisposition, OutreachTargetKind},
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::{AppState, IDEMPOTENCY_KEY, Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Debug, Serialize)]
struct OverviewResponse<T> {
    runtime_enabled: bool,
    #[serde(flatten)]
    overview: T,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityRequest {
    enabled: bool,
    autonomy_level: AutonomyLevel,
    minimum_confidence_basis_points: u16,
    max_actions_24h: u32,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MerchProductEconomicsRequest {
    product_id: Uuid,
    minimum_price_minor: i64,
    maximum_price_minor: i64,
    unit_cost_minor: Option<i64>,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingTargetRequest {
    target_id: Option<Uuid>,
    city_id: Uuid,
    target_kind: BookingTargetKind,
    display_name: String,
    contact_email: String,
    capacity: Option<u32>,
    priority: u16,
    relationship_score: u16,
    active: bool,
    accepts_booking: bool,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TicketAllocationGuardrailRequest {
    ticket_type_id: Uuid,
    minimum_capacity: u32,
    maximum_capacity: u32,
    step_capacity: u32,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionCampaignStateRequest {
    provider: String,
    external_campaign_key: String,
    event_id: Option<Uuid>,
    currency: String,
    current_daily_budget_minor: i64,
    minimum_daily_budget_minor: i64,
    maximum_daily_budget_minor: i64,
    spend_last_7d_minor: i64,
    spend_month_to_date_minor: i64,
    attributed_revenue_last_7d_minor: i64,
    active: bool,
    last_budget_change_at: Option<OffsetDateTime>,
    observed_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionBudgetGuardrailRequest {
    currency: String,
    maximum_total_daily_budget_minor: i64,
    maximum_monthly_spend_minor: i64,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityMarketSignalRequest {
    source: String,
    city_id: Uuid,
    signal_kind: CityMarketSignalKind,
    score_basis_points: u16,
    confidence_basis_points: u16,
    observed_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingReplyRequest {
    disposition: BookingReplyDisposition,
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachTargetRequest {
    target_id: Option<Uuid>,
    target_kind: OutreachTargetKind,
    display_name: String,
    contact_email: String,
    priority: u16,
    relationship_score: u16,
    active: bool,
    verified: bool,
    accepts_outreach: bool,
    do_not_contact: bool,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachOpportunityRequest {
    opportunity_id: Option<Uuid>,
    target_id: Uuid,
    source: String,
    subject_kind: String,
    subject_key: String,
    template_key: String,
    relevance_basis_points: u16,
    confidence_basis_points: u16,
    active: bool,
    observed_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachReplyRequest {
    opportunity_id: Option<Uuid>,
    disposition: OutreachReplyDisposition,
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePlanRequest {
    release_id: Option<Uuid>,
    source_key: String,
    title: String,
    release_at: OffsetDateTime,
    listen_url: Option<String>,
    active: bool,
    assets_ready: bool,
    communication_enabled: bool,
    press_enabled: bool,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOpportunityRequest {
    opportunity_id: Option<Uuid>,
    opportunity_kind: TeamOpportunityKind,
    source: String,
    external_key: String,
    title: String,
    organization: String,
    destination_url: Option<String>,
    contact_email: Option<String>,
    verified_destination: bool,
    fit_basis_points: u16,
    reputation_basis_points: u16,
    confidence_basis_points: u16,
    currency: String,
    expected_fee_minor: i64,
    estimated_cost_minor: i64,
    application_fee_minor: i64,
    requires_contract: bool,
    exclusive: bool,
    eligible: bool,
    funding_amount_minor: i64,
    own_contribution_minor: i64,
    deadline: Option<OffsetDateTime>,
    #[serde(default)]
    metadata: serde_json::Value,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOpportunityProgressRequest {
    progress: TeamOpportunityProgress,
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentSourceRequest {
    source_id: Option<Uuid>,
    source_kind: ContentSourceKind,
    source_key: String,
    title: String,
    occurred_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    metadata: serde_json::Value,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentVariantRequest {
    key: String,
    allocation_basis_points: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentRequest {
    slug: String,
    metric: ExperimentMetric,
    variants: Vec<ExperimentVariantRequest>,
    start: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentObservationRequest {
    experiment_id: Uuid,
    variant_id: Uuid,
    exposures_delta: u32,
    conversions_delta: u32,
    value_minor_delta: i64,
    observed_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentAssignmentRequest {
    assignment_key: String,
}

const PROMOTION_STATE_MAX_TTL: Duration = Duration::hours(24);
const PROMOTION_STATE_CLOCK_SKEW: Duration = Duration::minutes(5);
const MARKET_SIGNAL_MAX_TTL: Duration = Duration::days(7);

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

pub async fn set_authority(
    State(state): State<AppState>,
    Path(context): Path<String>,
    headers: HeaderMap,
    Json(request): Json<AuthorityRequest>,
) -> Response {
    let context = match parse_context(&context) {
        Some(context) => context,
        None => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
    };
    if request.expected_version <= 0 || !(1..=1000).contains(&request.max_actions_24h) {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let minimum_confidence =
        match Confidence::from_basis_points(request.minimum_confidence_basis_points) {
            Ok(value) => value,
            Err(_) => {
                return Problem::bad_request(request_id(&headers))
                    .private()
                    .into_response();
            }
        };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .set_authority(
            state.ops.workspace_id(),
            SetAutopilotAuthority {
                context,
                enabled: request.enabled,
                autonomy_level: request.autonomy_level,
                minimum_confidence,
                max_actions_24h: request.max_actions_24h,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_booking_target(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BookingTargetRequest>,
) -> Response {
    if request.expected_version < 0
        || (request.expected_version > 0 && request.target_id.is_none())
        || request.priority > 100
        || request.relationship_score > 100
        || request
            .capacity
            .is_some_and(|capacity| capacity == 0 || capacity > 100_000)
        || !valid_booking_name(&request.display_name)
        || !valid_booking_email(&request.contact_email)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertBookingTarget {
        target_id: request.target_id.map(BookingTargetId::from_uuid),
        city_id: CityId::from_uuid(request.city_id),
        kind: request.target_kind,
        display_name: request.display_name,
        contact_email: request.contact_email,
        capacity: request.capacity,
        priority: request.priority,
        relationship_score: request.relationship_score,
        active: request.active,
        accepts_booking: request.accepts_booking,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_booking_target(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_ticket_allocation_guardrail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TicketAllocationGuardrailRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if request.expected_version < 0
        || request.minimum_capacity == 0
        || request.maximum_capacity < request.minimum_capacity
        || request.step_capacity == 0
        || request.step_capacity > request.maximum_capacity
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_ticket_allocation_guardrail(
            state.ops.workspace_id(),
            UpsertTicketAllocationGuardrail {
                ticket_type_id: TicketTypeId::from_uuid(request.ticket_type_id),
                minimum_capacity: request.minimum_capacity,
                maximum_capacity: request.maximum_capacity,
                step_capacity: request.step_capacity,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_merch_product_economics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MerchProductEconomicsRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if request.expected_version < 0
        || request.minimum_price_minor < 0
        || request.maximum_price_minor < request.minimum_price_minor
        || request
            .unit_cost_minor
            .is_some_and(|cost| cost < 0 || cost > request.maximum_price_minor)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_merch_product_economics(
            state.ops.workspace_id(),
            UpsertMerchProductEconomics {
                product_id: MerchProductId::from_uuid(request.product_id),
                minimum_price_minor: request.minimum_price_minor,
                maximum_price_minor: request.maximum_price_minor,
                unit_cost_minor: request.unit_cost_minor,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_promotion_budget_guardrail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PromotionBudgetGuardrailRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !valid_currency(&request.currency)
        || request.maximum_total_daily_budget_minor <= 0
        || request.maximum_monthly_spend_minor <= 0
        || request.expected_version < 0
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_promotion_budget_guardrail(
            state.ops.workspace_id(),
            UpsertPromotionBudgetGuardrail {
                currency: request.currency,
                maximum_total_daily_budget_minor: request.maximum_total_daily_budget_minor,
                maximum_monthly_spend_minor: request.maximum_monthly_spend_minor,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_promotion_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PromotionCampaignStateRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let command = match validate_promotion_state(request) {
        Ok(command) => command,
        Err(()) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_promotion_campaign_state(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_city_market_signal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CityMarketSignalRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let command = match validate_city_market_signal(request) {
        Ok(command) => command,
        Err(()) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_city_market_signal(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn record_booking_reply(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<BookingReplyRequest>,
) -> Response {
    let Ok(target_id) = Uuid::parse_str(&target_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    if matches!(request.disposition, BookingReplyDisposition::None) {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = RecordBookingReply {
        target_id: BookingTargetId::from_uuid(target_id),
        disposition: request.disposition,
        occurred_at: request.occurred_at,
    };
    match state
        .autopilot
        .record_booking_reply(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_outreach_target(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OutreachTargetRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.target_id.is_none())
        || request.priority > 100
        || request.relationship_score > 100
        || !valid_booking_name(&request.display_name)
        || !valid_booking_email(&request.contact_email);
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }

    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertOutreachTarget {
        target_id: request.target_id.map(OutreachTargetId::from_uuid),
        kind: request.target_kind,
        display_name: request.display_name,
        contact_email: request.contact_email,
        priority: request.priority,
        relationship_score: request.relationship_score,
        active: request.active,
        verified: request.verified,
        accepts_outreach: request.accepts_outreach,
        do_not_contact: request.do_not_contact,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_outreach_target(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_outreach_opportunity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OutreachOpportunityRequest>,
) -> Response {
    let invalid = !valid_market_source(&request.source)
        || request.subject_key.trim().is_empty()
        || request.subject_key.len() > 200
        || request.template_key.trim().is_empty()
        || request.template_key.len() > 160
        || !matches!(
            request.subject_kind.as_str(),
            "release" | "event" | "catalogue" | "band"
        )
        || request.relevance_basis_points > 10_000
        || request.confidence_basis_points > 10_000
        || request.expires_at <= request.observed_at
        || request.expires_at - request.observed_at > Duration::days(90);
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let confidence = match Confidence::from_basis_points(request.confidence_basis_points) {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertOutreachOpportunity {
        opportunity_id: request.opportunity_id.map(OutreachOpportunityId::from_uuid),
        target_id: OutreachTargetId::from_uuid(request.target_id),
        source: request.source,
        subject_kind: request.subject_kind,
        subject_key: request.subject_key,
        template_key: request.template_key,
        relevance_basis_points: request.relevance_basis_points,
        confidence,
        active: request.active,
        observed_at: request.observed_at,
        expires_at: request.expires_at,
    };
    match state
        .autopilot
        .upsert_outreach_opportunity(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn record_outreach_reply(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<OutreachReplyRequest>,
) -> Response {
    let Ok(target_id) = Uuid::parse_str(&target_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = RecordOutreachReply {
        target_id: OutreachTargetId::from_uuid(target_id),
        opportunity_id: request.opportunity_id.map(OutreachOpportunityId::from_uuid),
        disposition: request.disposition,
        occurred_at: request.occurred_at,
    };
    match state
        .autopilot
        .record_outreach_reply(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_release_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ReleasePlanRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.release_id.is_none())
        || request.source_key.trim().is_empty()
        || request.source_key.len() > 160
        || request.title.trim().is_empty()
        || request.title.len() > 240
        || request
            .listen_url
            .as_ref()
            .is_some_and(|url| url.trim().is_empty() || url.len() > 1000);
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertReleasePlan {
        release_id: request.release_id.map(ReleasePlanId::from_uuid),
        source_key: request.source_key,
        title: request.title,
        release_at: request.release_at,
        listen_url: request.listen_url,
        active: request.active,
        assets_ready: request.assets_ready,
        communication_enabled: request.communication_enabled,
        press_enabled: request.press_enabled,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_release_plan(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_team_opportunity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TeamOpportunityRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.opportunity_id.is_none())
        || !valid_market_source(&request.source)
        || request.external_key.trim().is_empty()
        || request.external_key.len() > 240
        || request.title.trim().is_empty()
        || request.title.len() > 240
        || request.organization.trim().is_empty()
        || request.organization.len() > 240
        || request
            .destination_url
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > 1000)
        || request
            .contact_email
            .as_ref()
            .is_some_and(|value| !valid_booking_email(value))
        || request.fit_basis_points > 10_000
        || request.reputation_basis_points > 10_000
        || request.confidence_basis_points > 10_000
        || !valid_currency(&request.currency)
        || request.expected_fee_minor < 0
        || request.estimated_cost_minor < 0
        || request.application_fee_minor < 0
        || request.funding_amount_minor < 0
        || request.own_contribution_minor < 0
        || !request.metadata.is_object()
        || (matches!(request.opportunity_kind, TeamOpportunityKind::Funding)
            && request.deadline.is_none());
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let confidence = match Confidence::from_basis_points(request.confidence_basis_points) {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertTeamOpportunity {
        opportunity_id: request.opportunity_id.map(TeamOpportunityId::from_uuid),
        kind: request.opportunity_kind,
        source: request.source,
        external_key: request.external_key,
        title: request.title,
        organization: request.organization,
        destination_url: request.destination_url,
        contact_email: request.contact_email,
        verified_destination: request.verified_destination,
        fit_basis_points: request.fit_basis_points,
        reputation_basis_points: request.reputation_basis_points,
        confidence,
        currency: request.currency,
        expected_fee_minor: request.expected_fee_minor,
        estimated_cost_minor: request.estimated_cost_minor,
        application_fee_minor: request.application_fee_minor,
        requires_contract: request.requires_contract,
        exclusive: request.exclusive,
        eligible: request.eligible,
        funding_amount_minor: request.funding_amount_minor,
        own_contribution_minor: request.own_contribution_minor,
        deadline: request.deadline,
        metadata: request.metadata,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_team_opportunity(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn record_team_opportunity_progress(
    State(state): State<AppState>,
    Path(opportunity_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<TeamOpportunityProgressRequest>,
) -> Response {
    let Ok(opportunity_id) = Uuid::parse_str(&opportunity_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = RecordTeamOpportunityProgress {
        opportunity_id: TeamOpportunityId::from_uuid(opportunity_id),
        progress: request.progress,
        occurred_at: request.occurred_at,
    };
    match state
        .autopilot
        .record_team_opportunity_progress(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_content_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ContentSourceRequest>,
) -> Response {
    let invalid = request.expected_version < 0
        || (request.expected_version > 0 && request.source_id.is_none())
        || request.source_key.trim().is_empty()
        || request.source_key.len() > 200
        || request.title.trim().is_empty()
        || request.title.len() > 240
        || request.expires_at <= request.occurred_at
        || request.expires_at - request.occurred_at > Duration::days(90)
        || !request.metadata.is_object();
    if invalid {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertContentSource {
        source_id: request.source_id.map(ContentSourceId::from_uuid),
        kind: request.source_kind,
        source_key: request.source_key,
        title: request.title,
        occurred_at: request.occurred_at,
        expires_at: request.expires_at,
        metadata: request.metadata,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_content_source(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn create_experiment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExperimentRequest>,
) -> Response {
    if request.slug.is_empty()
        || request.slug.len() > 128
        || !(2..=8).contains(&request.variants.len())
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = CreateExperiment {
        slug: request.slug,
        metric: request.metric,
        variants: request
            .variants
            .into_iter()
            .map(|variant| CreateExperimentVariant {
                key: variant.key,
                allocation_basis_points: variant.allocation_basis_points,
            })
            .collect(),
        start: request.start,
    };
    match state
        .autopilot
        .create_experiment(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::CREATED, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn assign_experiment(
    State(state): State<AppState>,
    Path(experiment_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ExperimentAssignmentRequest>,
) -> Response {
    let Ok(experiment_id) = Uuid::parse_str(&experiment_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    if request.assignment_key.trim().is_empty() || request.assignment_key.len() > 200 {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }

    match assign_experiment_variant(
        &state.autopilot,
        state.ops.workspace_id(),
        ExperimentId::from_uuid(experiment_id),
        &request.assignment_key,
    )
    .await
    {
        Ok(assignment) => private_json(StatusCode::OK, assignment),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn record_experiment_observation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExperimentObservationRequest>,
) -> Response {
    if request.conversions_delta > request.exposures_delta || request.value_minor_delta < 0 {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = ExperimentObservation {
        experiment_id: ExperimentId::from_uuid(request.experiment_id),
        variant_id: ExperimentVariantId::from_uuid(request.variant_id),
        exposures_delta: request.exposures_delta,
        conversions_delta: request.conversions_delta,
        value_minor_delta: request.value_minor_delta,
        observed_at: request.observed_at,
    };
    match state
        .autopilot
        .record_experiment_observation(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn approve_action(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    mutate_action(state, headers, action_id, true).await
}

pub async fn cancel_action(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    mutate_action(state, headers, action_id, false).await
}

async fn mutate_action(
    state: AppState,
    headers: HeaderMap,
    action_id: String,
    approve: bool,
) -> Response {
    let action_id = match Uuid::parse_str(&action_id) {
        Ok(value) => AutopilotActionId::from_uuid(value),
        Err(_) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
    };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let result = if approve {
        state
            .autopilot
            .approve_action(
                state.ops.workspace_id(),
                action_id,
                &idempotency_key,
                request_id_value.as_ref(),
            )
            .await
    } else {
        state
            .autopilot
            .cancel_action(
                state.ops.workspace_id(),
                action_id,
                &idempotency_key,
                request_id_value.as_ref(),
            )
            .await
    };
    match result {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

fn validate_city_market_signal(
    request: CityMarketSignalRequest,
) -> Result<UpsertCityMarketSignal, ()> {
    if !valid_market_source(&request.source)
        || request.score_basis_points > 10_000
        || request.confidence_basis_points > 10_000
        || request.expires_at <= request.observed_at
        || request.expires_at - request.observed_at > MARKET_SIGNAL_MAX_TTL
        || request.observed_at > OffsetDateTime::now_utc() + PROMOTION_STATE_CLOCK_SKEW
    {
        return Err(());
    }
    let confidence =
        Confidence::from_basis_points(request.confidence_basis_points).map_err(|_| ())?;
    Ok(UpsertCityMarketSignal {
        source: request.source,
        city_id: CityId::from_uuid(request.city_id),
        kind: request.signal_kind,
        score_basis_points: request.score_basis_points,
        confidence,
        observed_at: request.observed_at,
        expires_at: request.expires_at,
    })
}

fn validate_promotion_state(
    request: PromotionCampaignStateRequest,
) -> Result<UpsertPromotionCampaignState, ()> {
    if !valid_provider(&request.provider)
        || !valid_external_key(&request.external_campaign_key)
        || !valid_currency(&request.currency)
        || request.minimum_daily_budget_minor <= 0
        || request.current_daily_budget_minor < request.minimum_daily_budget_minor
        || request.maximum_daily_budget_minor < request.current_daily_budget_minor
        || request.spend_last_7d_minor < 0
        || request.spend_month_to_date_minor < 0
        || request.attributed_revenue_last_7d_minor < 0
        || request.expires_at <= request.observed_at
        || request.expires_at - request.observed_at > PROMOTION_STATE_MAX_TTL
        || request.observed_at > OffsetDateTime::now_utc() + PROMOTION_STATE_CLOCK_SKEW
        || request
            .last_budget_change_at
            .is_some_and(|value| value > request.observed_at)
    {
        return Err(());
    }
    Ok(UpsertPromotionCampaignState {
        provider: request.provider,
        external_campaign_key: request.external_campaign_key,
        event_id: request.event_id.map(EventId::from_uuid),
        currency: request.currency,
        current_daily_budget_minor: request.current_daily_budget_minor,
        minimum_daily_budget_minor: request.minimum_daily_budget_minor,
        maximum_daily_budget_minor: request.maximum_daily_budget_minor,
        spend_last_7d_minor: request.spend_last_7d_minor,
        spend_month_to_date_minor: request.spend_month_to_date_minor,
        attributed_revenue_last_7d_minor: request.attributed_revenue_last_7d_minor,
        active: request.active,
        last_budget_change_at: request.last_budget_change_at,
        observed_at: request.observed_at,
        expires_at: request.expires_at,
    })
}

fn valid_booking_name(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed.len() <= 200 && trimmed.chars().all(|ch| !ch.is_control())
}

fn valid_booking_email(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 320 || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = trimmed.rsplit_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

fn valid_market_source(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn valid_provider(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_external_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value.chars().all(|character| !character.is_control())
}

fn valid_currency(value: &str) -> bool {
    value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn parse_context(value: &str) -> Option<AutopilotContext> {
    match value {
        "ticket_yield" => Some(AutopilotContext::TicketYield),
        "fan_lifecycle" => Some(AutopilotContext::FanLifecycle),
        "campaign_lifecycle" => Some(AutopilotContext::CampaignLifecycle),
        "merchandising" => Some(AutopilotContext::Merchandising),
        "merch_pricing" => Some(AutopilotContext::MerchPricing),
        "merch_bundle" => Some(AutopilotContext::MerchBundle),
        "booking_opportunity" => Some(AutopilotContext::BookingOpportunity),
        "outreach" => Some(AutopilotContext::Outreach),
        "content_supply" => Some(AutopilotContext::ContentSupply),
        "promotion_budget" => Some(AutopilotContext::PromotionBudget),
        "experimentation" => Some(AutopilotContext::Experimentation),
        "show_operations" => Some(AutopilotContext::ShowOperations),
        "release" => Some(AutopilotContext::Release),
        "live_opportunity" => Some(AutopilotContext::LiveOpportunity),
        "funding" => Some(AutopilotContext::Funding),
        _ => None,
    }
}

#[allow(clippy::result_large_err)]
fn parse_idempotency_key(headers: &HeaderMap) -> Result<IdempotencyKey, Response> {
    let value = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok());
    value.ok_or_else(|| {
        Problem::bad_request(request_id(headers))
            .private()
            .into_response()
    })
}

fn parsed_request_id(headers: &HeaderMap) -> Option<RequestId> {
    request_id(headers).and_then(|value| RequestId::parse(value).ok())
}

fn repository_problem(error: RepositoryError, request_id: Option<String>) -> Response {
    match error {
        RepositoryError::Unavailable => Problem::service_unavailable(request_id).private(),
        RepositoryError::NotFound => Problem::not_found(request_id).private(),
        RepositoryError::Conflict => Problem::conflict(request_id).private(),
        RepositoryError::Unexpected => Problem::internal(request_id).private(),
    }
    .into_response()
}

fn private_json<T: serde::Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(body)).into_response()
}
