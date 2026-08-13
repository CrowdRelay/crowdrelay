//! Closed-loop runtime ingress for external executors and deployment reporters.
//! Transport validation lives here; authority and persistence remain in the
//! application/infra layers.

use super::{private_json, repository_problem};
use crate::{AppState, Problem, request_id};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crowdrelay_application::autopilot::{
    AutopilotRuntimeRepository, ClaimExecution, ExecutorCapability, ExecutorReportStatus,
    RecordExecutionReport, RecordExecutorHeartbeat, RecordRumSample, UpsertReleaseComponent,
};
use crowdrelay_domain::AutopilotActionId;
use serde::Deserialize;
use std::collections::HashSet;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const MAX_HEARTBEAT_TTL: Duration = Duration::hours(2);
const CLOCK_SKEW: Duration = Duration::minutes(5);
const MAX_RUM_AGE: Duration = Duration::days(1);
const MAX_EXECUTION_REPORT_AGE: Duration = Duration::days(7);
const RELEASE_COMPONENTS: [&str; 6] = [
    "crowdrelay-api",
    "crowdrelay-worker",
    "virya-www",
    "synesthesia",
    "virya-signal",
    "n8n",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReportRequest {
    receipt_key: String,
    executor_id: String,
    status: ExecutorReportStatus,
    claim_token: Option<Uuid>,
    provider_reference: Option<String>,
    error_kind: Option<String>,
    #[serde(default = "empty_metadata")]
    metadata: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionClaimRequest {
    executor_id: String,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorHeartbeatRequest {
    executor_id: String,
    version: String,
    manifest_sha: String,
    capabilities: Vec<ExecutorCapability>,
    #[serde(default = "empty_metadata")]
    metadata: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseComponentRequest {
    component_key: String,
    #[serde(default = "production_environment")]
    environment: String,
    source_sha: String,
    artifact_digest: Option<String>,
    deploy_ref: Option<String>,
    version: Option<String>,
    manifest_sha: Option<String>,
    #[serde(default = "empty_metadata")]
    metadata: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RumRequest {
    surface: String,
    metric_key: String,
    value: f64,
    route: Option<String>,
    device_class: Option<String>,
    release: Option<String>,
    #[serde(default = "empty_metadata")]
    metadata: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
}

fn production_environment() -> String {
    "production".to_owned()
}

fn empty_metadata() -> serde_json::Value {
    serde_json::json!({})
}

fn text_ok(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}

fn object_metadata(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|map| map.len() <= 24)
        && serde_json::to_vec(value).is_ok_and(|encoded| encoded.len() <= 2_048)
}

fn time_is_current(value: OffsetDateTime, now: OffsetDateTime, max_age: Duration) -> bool {
    value >= now - max_age && value <= now + CLOCK_SKEW
}

pub async fn execution_claim(
    State(state): State<AppState>,
    Path(action_id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<ExecutionClaimRequest>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let now = OffsetDateTime::now_utc();
    if !text_ok(&request.executor_id, 120)
        || !time_is_current(request.occurred_at, now, MAX_EXECUTION_REPORT_AGE)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .claim_execution(
            state.ops.workspace_id(),
            ClaimExecution {
                action_id: AutopilotActionId::from_uuid(action_id),
                executor_id: request.executor_id,
                occurred_at: request.occurred_at,
            },
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn execution_report(
    State(state): State<AppState>,
    Path(action_id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<ExecutionReportRequest>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let now = OffsetDateTime::now_utc();
    if !text_ok(&request.receipt_key, 200)
        || !text_ok(&request.executor_id, 120)
        || request
            .provider_reference
            .as_ref()
            .is_some_and(|value| value.len() > 240)
        || request
            .error_kind
            .as_ref()
            .is_some_and(|value| value.len() > 96)
        || !object_metadata(&request.metadata)
        || !time_is_current(request.occurred_at, now, MAX_EXECUTION_REPORT_AGE)
        || (request.status == ExecutorReportStatus::Failed && request.error_kind.is_none())
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .record_execution_report(
            state.ops.workspace_id(),
            RecordExecutionReport {
                action_id: AutopilotActionId::from_uuid(action_id),
                receipt_key: request.receipt_key,
                executor_id: request.executor_id,
                status: request.status,
                claim_token: request.claim_token,
                provider_reference: request.provider_reference,
                error_kind: request.error_kind,
                metadata: request.metadata,
                occurred_at: request.occurred_at,
            },
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn provider_action(
    State(state): State<AppState>,
    Path(provider_reference): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let executor_id = headers
        .get("x-virya-executor-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !text_ok(executor_id, 120) || !text_ok(&provider_reference, 240) {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .find_provider_action(state.ops.workspace_id(), executor_id, &provider_reference)
        .await
    {
        Ok(Some(result)) => private_json(StatusCode::OK, result),
        Ok(None) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn executor_heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExecutorHeartbeatRequest>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let now = OffsetDateTime::now_utc();
    let unique = request
        .capabilities
        .iter()
        .map(|item| item.capability.as_str())
        .collect::<HashSet<_>>();
    let capabilities_valid = !request.capabilities.is_empty()
        && request.capabilities.len() <= 64
        && unique.len() == request.capabilities.len()
        && request
            .capabilities
            .iter()
            .all(|item| text_ok(&item.capability, 120) && text_ok(&item.version, 40));
    if !text_ok(&request.executor_id, 120)
        || !text_ok(&request.version, 80)
        || !text_ok(&request.manifest_sha, 128)
        || !capabilities_valid
        || !object_metadata(&request.metadata)
        || !time_is_current(request.observed_at, now, CLOCK_SKEW)
        || request.expires_at <= request.observed_at
        || request.expires_at > request.observed_at + MAX_HEARTBEAT_TTL
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .record_executor_heartbeat(
            state.ops.workspace_id(),
            RecordExecutorHeartbeat {
                executor_id: request.executor_id,
                version: request.version,
                manifest_sha: request.manifest_sha,
                capabilities: request.capabilities,
                metadata: request.metadata,
                observed_at: request.observed_at,
                expires_at: request.expires_at,
            },
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn release_component(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ReleaseComponentRequest>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    let now = OffsetDateTime::now_utc();
    if !RELEASE_COMPONENTS.contains(&request.component_key.as_str())
        || request.environment != "production"
        || !text_ok(&request.source_sha, 128)
        || request
            .artifact_digest
            .as_ref()
            .is_some_and(|value| value.len() > 200)
        || request
            .deploy_ref
            .as_ref()
            .is_some_and(|value| value.len() > 240)
        || request
            .version
            .as_ref()
            .is_some_and(|value| value.len() > 80)
        || request
            .manifest_sha
            .as_ref()
            .is_some_and(|value| value.len() > 128)
        || !object_metadata(&request.metadata)
        || !time_is_current(request.observed_at, now, MAX_RUM_AGE)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .upsert_release_component(
            state.ops.workspace_id(),
            UpsertReleaseComponent {
                component_key: request.component_key,
                environment: request.environment,
                source_sha: request.source_sha,
                artifact_digest: request.artifact_digest,
                deploy_ref: request.deploy_ref,
                version: request.version,
                manifest_sha: request.manifest_sha,
                metadata: request.metadata,
                observed_at: request.observed_at,
            },
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn release_ledger(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id(&headers))
            .private()
            .into_response();
    }
    match state
        .autopilot
        .load_release_ledger(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn rum(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RumRequest>,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let surface_ok = matches!(
        request.surface.as_str(),
        "virya_www" | "synesthesia" | "virya_signal"
    );
    let metric_ok = match request.surface.as_str() {
        "virya_www" => matches!(
            request.metric_key.as_str(),
            "lcp_ms" | "inp_ms" | "cls_milli" | "ttfb_ms"
        ),
        "synesthesia" => matches!(
            request.metric_key.as_str(),
            "boot_interactive_ms" | "room_load_ms" | "transition_ms" | "frame_hitch_ms"
        ),
        "virya_signal" => matches!(
            request.metric_key.as_str(),
            "cold_start_ms" | "api_latency_ms" | "screen_transition_ms"
        ),
        _ => false,
    };
    if !surface_ok
        || !metric_ok
        || !request.value.is_finite()
        || request.value < 0.0
        || request.value > 86_400_000.0
        || request
            .route
            .as_ref()
            .is_some_and(|value| value.len() > 160)
        || request
            .device_class
            .as_ref()
            .is_some_and(|value| value.len() > 40)
        || request
            .release
            .as_ref()
            .is_some_and(|value| value.len() > 128)
        || !object_metadata(&request.metadata)
        || !time_is_current(request.observed_at, now, MAX_RUM_AGE)
    {
        return Problem::bad_request(request_id(&headers)).into_response();
    }
    match state
        .autopilot
        .record_rum_sample(
            state.ops.workspace_id(),
            RecordRumSample {
                surface: request.surface,
                metric_key: request.metric_key,
                value: request.value,
                route: request.route,
                device_class: request.device_class,
                release: request.release,
                metadata: request.metadata,
                observed_at: request.observed_at,
            },
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}
