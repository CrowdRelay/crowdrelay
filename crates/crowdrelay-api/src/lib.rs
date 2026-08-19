#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used,
    )
)]
#![deny(clippy::dbg_macro)]

//! HTTP transport for CrowdRelay.
//!
//! This crate owns routing, protocol-level responses, and HTTP middleware. Domain
//! and application logic belongs in their respective crates.

use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, MatchedPath, State},
    http::{
        HeaderMap, HeaderValue, Method, Request, StatusCode,
        header::{
            ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderName, IF_NONE_MATCH,
            InvalidHeaderValue,
        },
    },
    middleware::{Next, from_fn, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use crowdrelay_infra::{
    area_admin::PostgresAreaAdminRepository, autopilot::PostgresAutopilotRepository, database,
    ecosystem::PostgresEcosystemRepository,
};
use serde::Serialize;
use sqlx::PgPool;
use tower::ServiceBuilder;
use tower_http::{
    cors::CorsLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::{DefaultOnResponse, TraceLayer},
};
use tracing::{Level, info_span};

mod accounting;
mod acquisition;
mod admission;
mod area;
mod area_admin;
mod audience;
mod autopilot;
mod beacon_signal;
mod commerce;
mod concert_qr;
mod ecosystem;
mod event_copy;
mod events;
mod fan_context;
mod fan_lifecycle;
mod fan_privacy;
mod http_metrics;
mod meta;
mod mobile_fan;
mod ops;
mod ops_routes;
mod ops_summary;
mod proofs;
mod push;
mod referrals;
mod releases;
mod routing;
mod security;
mod staff_sessions;
mod synesthesia;
pub mod tenant;
mod ticket_qr;
mod ticketing;

pub use acquisition::{
    AcquisitionState, AcquisitionStateArgs, ClickMetricsReader, ClickMetricsSnapshot,
    ClickSubmitter,
};
pub use admission::{AdmissionState, AdmissionStateArgs};
pub use concert_qr::ConcertQrState;
pub use events::{
    EventActionMetricsReader, EventActionMetricsSnapshot, EventActionSubmitter, EventState,
};
pub use fan_lifecycle::FanLifecycleState;
pub use ops::OpsState;
pub use push::PushPublicState;
pub use referrals::ReferralState;
pub use ticketing::TicketingState;

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const X_CROWDRELAY_CORRELATION_ID: HeaderName =
    HeaderName::from_static("x-crowdrelay-correlation-id");
const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");
const X_CROWDRELAY_RELEASE: HeaderName = HeaderName::from_static("x-crowdrelay-release");
static HTTP_METRICS: OnceLock<Arc<http_metrics::HttpMetrics>> = OnceLock::new();

fn http_metrics() -> &'static Arc<http_metrics::HttpMetrics> {
    HTTP_METRICS.get_or_init(|| Arc::new(http_metrics::HttpMetrics::default()))
}
const MAX_PUBLIC_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivilegedAuthorization {
    Admin,
    Operator,
    Commerce,
    AreaManagement,
}

/// Shared HTTP state assembled by the API composition root.
#[derive(Clone)]
pub struct AppState {
    database: PgPool,
    readiness_timeout: Duration,
    pub(crate) acquisition: AcquisitionState,
    pub(crate) referrals: ReferralState,
    pub(crate) events: EventState,
    pub(crate) admission: AdmissionState,
    pub(crate) concert_qr: ConcertQrState,
    pub(crate) fan_lifecycle: FanLifecycleState,
    pub(crate) ticketing: TicketingState,
    pub(crate) area_admin: crowdrelay_application::AreaAdminService,
    area_management_api_key_sha256: Option<[u8; 32]>,
    pub(crate) ops: OpsState,
    pub(crate) autopilot: PostgresAutopilotRepository,
    pub(crate) autopilot_runtime_enabled: bool,
    /// Built from the pool this state already owns, so callers do not grow
    /// another constructor argument for it.
    pub(crate) ecosystem: PostgresEcosystemRepository,
    pub(crate) push: push::PushPublicState,
    #[expect(dead_code)] // Tenants are disabled, but prepared for future use
    pub(crate) tenant: tenant::TenantProfile,
}

impl AppState {
    /// Creates the complete API state from validated repositories and route state.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database: PgPool,
        readiness_timeout: Duration,
        acquisition: AcquisitionState,
        referrals: ReferralState,
        events: EventState,
        admission: AdmissionState,
        concert_qr: ConcertQrState,
        fan_lifecycle: FanLifecycleState,
        ticketing: TicketingState,
        area_management_api_key_sha256: Option<[u8; 32]>,
        ops: OpsState,
        autopilot: PostgresAutopilotRepository,
        autopilot_runtime_enabled: bool,
        push: push::PushPublicState,
        tenant: tenant::TenantProfile,
    ) -> Self {
        let ecosystem = PostgresEcosystemRepository::new(database.clone());
        let area_admin = crowdrelay_application::AreaAdminService::new(Arc::new(
            PostgresAreaAdminRepository::new(database.clone()),
        ));
        Self {
            database,
            readiness_timeout,
            acquisition,
            referrals,
            events,
            admission,
            concert_qr,
            fan_lifecycle,
            ticketing,
            area_admin,
            area_management_api_key_sha256,
            ops,
            autopilot,
            autopilot_runtime_enabled,
            ecosystem,
            push,
            tenant,
        }
    }
}

/// HTTP-level configuration validated before the router starts.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// CORS origins allowed to make credentialed requests.
    pub allowed_origins: Vec<HeaderValue>,
}

impl HttpConfig {
    /// Parses allowed CORS origins into HTTP header values.
    pub fn new(
        allowed_origins: impl IntoIterator<Item = String>,
    ) -> Result<Self, InvalidHeaderValue> {
        let allowed_origins = allowed_origins
            .into_iter()
            .map(|origin| HeaderValue::from_str(&origin))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { allowed_origins })
    }
}

/// Builds the HTTP router. Health probes are exposed both at the contract path
/// (`/v1/health/*`) and at an unversioned operational alias (`/health/*`).
pub fn router(state: AppState, config: HttpConfig) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(config.allowed_origins)
        .allow_credentials(true)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            ACCEPT,
            CONTENT_TYPE,
            AUTHORIZATION,
            IDEMPOTENCY_KEY,
            IF_NONE_MATCH,
            X_REQUEST_ID,
            X_CROWDRELAY_CORRELATION_ID,
        ])
        .expose_headers([
            CACHE_CONTROL,
            ETAG,
            X_REQUEST_ID,
            SERVER_TIMING.clone(),
            X_CROWDRELAY_RELEASE.clone(),
        ]);

    let middleware = ServiceBuilder::new()
        .layer(from_fn(measure_request))
        .layer(from_fn_with_state(state.clone(), normalize_request_id))
        .layer(SetRequestIdLayer::new(
            X_REQUEST_ID.clone(),
            MakeRequestUuid,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<Body>| {
                    let request_id = request
                        .headers()
                        .get(&X_REQUEST_ID)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("unknown");

                    info_span!(
                        "http.request",
                        request_id,
                        method = %request.method(),
                        path = request.uri().path()
                    )
                })
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(PropagateRequestIdLayer::new(X_REQUEST_ID.clone()))
        .layer(cors);

    routing::application_routes(state.clone())
        .merge(area_admin::router(state.clone()))
        .layer(from_fn_with_state(state, enforce_privileged_namespace))
        .layer(middleware)
}

fn is_area_management_path(path: &str) -> bool {
    path == "/v1/control-plane/area" || path.starts_with("/v1/control-plane/area/")
}

async fn enforce_privileged_namespace(
    State(_state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let authorization = request
        .extensions()
        .get::<PrivilegedAuthorization>()
        .copied();
    let authorized = if path.starts_with("/v1/admin/") {
        authorization == Some(PrivilegedAuthorization::Admin)
    } else if path.starts_with("/v1/staff/") {
        authorization == Some(PrivilegedAuthorization::Operator)
    } else if path.starts_with("/v1/commerce/") || path.starts_with("/v1/internal/") {
        authorization == Some(PrivilegedAuthorization::Commerce)
    } else if is_area_management_path(path) {
        authorization == Some(PrivilegedAuthorization::AreaManagement)
    } else {
        true
    };
    if !authorized {
        return Problem::unauthorized(request_id(request.headers()))
            .private()
            .into_response();
    }
    next.run(request).await
}

async fn measure_request(request: Request<Body>, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().as_str().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "<unmatched>".to_owned());
    let mut response = next.run(request).await;
    let elapsed_micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    http_metrics().record(elapsed_micros, response.status().as_u16());
    http_metrics().record_route(&method, &route, elapsed_micros, response.status().as_u16());
    let elapsed_ms = elapsed_micros as f64 / 1_000.0;
    if let Ok(value) = HeaderValue::from_str(&format!("app;dur={elapsed_ms:.2}")) {
        response.headers_mut().insert(SERVER_TIMING.clone(), value);
    }
    if let Ok(value) = HeaderValue::from_str(meta::release_identity()) {
        response
            .headers_mut()
            .insert(X_CROWDRELAY_RELEASE.clone(), value);
    }
    response
}

async fn normalize_request_id(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    request.headers_mut().remove(&X_REQUEST_ID);
    let path = request.uri().path();
    let privileged = path.starts_with("/v1/admin/")
        || path.starts_with("/v1/staff/")
        || path.starts_with("/v1/internal/")
        || path.starts_with("/v1/commerce/")
        || is_area_management_path(path);
    let authorization = if path.starts_with("/v1/admin/")
        && state.ticketing.admin_authorized(request.headers())
    {
        Some(PrivilegedAuthorization::Admin)
    } else if path.starts_with("/v1/staff/")
        && state.ticketing.operator_authorized(request.headers()).await
    {
        Some(PrivilegedAuthorization::Operator)
    } else if (path.starts_with("/v1/internal/") || path.starts_with("/v1/commerce/"))
        && state.ticketing.commerce_authorized(request.headers())
    {
        Some(PrivilegedAuthorization::Commerce)
    } else if is_area_management_path(path)
        && security::bearer_sha256_matches(request.headers(), state.area_management_api_key_sha256)
    {
        Some(PrivilegedAuthorization::AreaManagement)
    } else {
        None
    };
    if let Some(authorization) = authorization {
        request.extensions_mut().insert(authorization);
    }
    if privileged && authorization.is_some() {
        let correlation = request
            .headers()
            .get(&X_CROWDRELAY_CORRELATION_ID)
            .cloned()
            .filter(|value| {
                value.to_str().is_ok_and(|value| {
                    let value = value.trim();
                    (8..=128).contains(&value.len())
                        && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
                })
            });
        if let Some(correlation) = correlation {
            request
                .headers_mut()
                .insert(X_REQUEST_ID.clone(), correlation);
        }
    }
    request.headers_mut().remove(&X_CROWDRELAY_CORRELATION_ID);
    next.run(request).await
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn live() -> impl IntoResponse {
    no_store_json(StatusCode::OK, HealthResponse { status: "ok" })
}

async fn ready(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match database::ping(&state.database, state.readiness_timeout).await {
        Ok(()) => no_store_json(StatusCode::OK, HealthResponse { status: "ready" }).into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "readiness probe failed");

            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

async fn metrics(State(state): State<AppState>) -> Response {
    let http_snapshot = http_metrics().snapshot();
    let snapshot = state.acquisition.click_metrics_snapshot();
    let event_snapshot = state.events.metrics_snapshot();
    let ops_snapshot = state.ops.metrics_snapshot().await.unwrap_or_default();
    let mut body = format!(
        concat!(
            "# HELP crowdrelay_http_requests_total HTTP requests completed by the API.\n",
            "# TYPE crowdrelay_http_requests_total counter\n",
            "crowdrelay_http_requests_total {}\n",
            "# HELP crowdrelay_http_requests_4xx_total HTTP requests completed with a 4xx status.\n",
            "# TYPE crowdrelay_http_requests_4xx_total counter\n",
            "crowdrelay_http_requests_4xx_total {}\n",
            "# HELP crowdrelay_http_requests_5xx_total HTTP requests completed with a 5xx status.\n",
            "# TYPE crowdrelay_http_requests_5xx_total counter\n",
            "crowdrelay_http_requests_5xx_total {}\n",
            "# HELP crowdrelay_http_request_duration_seconds_sum Total request wall time.\n",
            "# TYPE crowdrelay_http_request_duration_seconds histogram\n",
            "crowdrelay_http_request_duration_seconds_sum {:.6}\n",
            "crowdrelay_http_request_duration_seconds_count {}\n",
            "",
            "crowdrelay_http_request_duration_bucket{{le=\"0.05\"}} {}\n",
            "crowdrelay_http_request_duration_bucket{{le=\"0.10\"}} {}\n",
            "crowdrelay_http_request_duration_bucket{{le=\"0.25\"}} {}\n",
            "crowdrelay_http_request_duration_bucket{{le=\"0.50\"}} {}\n",
            "crowdrelay_http_request_duration_bucket{{le=\"1.00\"}} {}\n",
            "crowdrelay_http_request_duration_bucket{{le=\"2.50\"}} {}\n",
            "crowdrelay_http_request_duration_bucket{{le=\"5.00\"}} {}\n",
            "crowdrelay_http_request_duration_bucket{{le=\"+Inf\"}} {}\n",
            "# HELP crowdrelay_click_events_queued_total Click events accepted by the bounded buffer.\n",
            "# TYPE crowdrelay_click_events_queued_total counter\n",
            "crowdrelay_click_events_queued_total {}\n",
            "# HELP crowdrelay_click_events_persisted_total Click events durably written to PostgreSQL.\n",
            "# TYPE crowdrelay_click_events_persisted_total counter\n",
            "crowdrelay_click_events_persisted_total {}\n",
            "# HELP crowdrelay_click_events_dropped_total Click events dropped under overload or shutdown.\n",
            "# TYPE crowdrelay_click_events_dropped_total counter\n",
            "crowdrelay_click_events_dropped_total {}\n",
            "# HELP crowdrelay_click_events_persistence_failed_total Click events dropped after a bounded persistence failure.\n",
            "# TYPE crowdrelay_click_events_persistence_failed_total counter\n",
            "crowdrelay_click_events_persistence_failed_total {}\n",
            "# HELP crowdrelay_event_actions_queued_total Event conversion actions accepted by the bounded buffer.\n",
            "# TYPE crowdrelay_event_actions_queued_total counter\n",
            "crowdrelay_event_actions_queued_total {}\n",
            "# HELP crowdrelay_event_actions_persisted_total Event conversion actions written to PostgreSQL.\n",
            "# TYPE crowdrelay_event_actions_persisted_total counter\n",
            "crowdrelay_event_actions_persisted_total {}\n",
            "# HELP crowdrelay_event_actions_dropped_total Event conversion actions dropped under overload or shutdown.\n",
            "# TYPE crowdrelay_event_actions_dropped_total counter\n",
            "crowdrelay_event_actions_dropped_total {}\n",
            "# HELP crowdrelay_event_actions_persistence_failed_total Event conversion actions lost after bounded persistence failure.\n",
            "# TYPE crowdrelay_event_actions_persistence_failed_total counter\n",
            "crowdrelay_event_actions_persistence_failed_total {}\n",
            "# HELP crowdrelay_legacy_area_claim_import_attempt_total Authorized calls reaching the enabled pre-PostgreSQL AREA claim import bridge.\n",
            "# TYPE crowdrelay_legacy_area_claim_import_attempt_total counter\n",
            "crowdrelay_legacy_area_claim_import_attempt_total {}\n",
            "# HELP crowdrelay_legacy_area_wallet_import_attempt_total Authorized calls reaching the enabled pre-PostgreSQL AREA wallet import bridge.\n",
            "# TYPE crowdrelay_legacy_area_wallet_import_attempt_total counter\n",
            "crowdrelay_legacy_area_wallet_import_attempt_total {}\n",
            "# HELP crowdrelay_legacy_area_claim_import_total Newly applied pre-PostgreSQL AREA legacy claims; idempotent replays do not increment it.\n",
            "# TYPE crowdrelay_legacy_area_claim_import_total counter\n",
            "crowdrelay_legacy_area_claim_import_total {}\n",
            "# HELP crowdrelay_legacy_area_wallet_import_total Newly applied pre-PostgreSQL AREA wallet migrations; idempotent replays do not increment it.\n",
            "# TYPE crowdrelay_legacy_area_wallet_import_total counter\n",
            "crowdrelay_legacy_area_wallet_import_total {}\n",
            "# HELP crowdrelay_legacy_static_staff_auth_total Requests authenticated with the deprecated global staff bearer instead of a device session.\n",
            "# TYPE crowdrelay_legacy_static_staff_auth_total counter\n",
            "crowdrelay_legacy_static_staff_auth_total {}\n",
            "# HELP crowdrelay_outbox_pending Current pending outbox events.\n",
            "# TYPE crowdrelay_outbox_pending gauge\n",
            "crowdrelay_outbox_pending {}\n",
            "# HELP crowdrelay_outbox_processing Current processing outbox events.\n",
            "# TYPE crowdrelay_outbox_processing gauge\n",
            "crowdrelay_outbox_processing {}\n",
            "# HELP crowdrelay_outbox_dead Current dead outbox events.\n",
            "# TYPE crowdrelay_outbox_dead gauge\n",
            "crowdrelay_outbox_dead {}\n",
            "# HELP crowdrelay_outbox_oldest_pending_seconds Age of the oldest ready pending outbox event.\n",
            "# TYPE crowdrelay_outbox_oldest_pending_seconds gauge\n",
            "crowdrelay_outbox_oldest_pending_seconds {}\n",
            "# HELP crowdrelay_webhook_delivery_pending Current pending webhook deliveries.\n",
            "# TYPE crowdrelay_webhook_delivery_pending gauge\n",
            "crowdrelay_webhook_delivery_pending {}\n",
            "# HELP crowdrelay_webhook_delivery_processing Current processing webhook deliveries.\n",
            "# TYPE crowdrelay_webhook_delivery_processing gauge\n",
            "crowdrelay_webhook_delivery_processing {}\n",
            "# HELP crowdrelay_webhook_delivery_dead Current dead webhook deliveries.\n",
            "# TYPE crowdrelay_webhook_delivery_dead gauge\n",
            "crowdrelay_webhook_delivery_dead {}\n",
            "# HELP crowdrelay_webhook_delivery_cancelled Current cancelled webhook deliveries (endpoint deactivated).\n",
            "# TYPE crowdrelay_webhook_delivery_cancelled gauge\n",
            "crowdrelay_webhook_delivery_cancelled {}\n",
            "# HELP crowdrelay_webhook_delivery_oldest_pending_seconds Age of the oldest ready pending webhook delivery.\n",
            "# TYPE crowdrelay_webhook_delivery_oldest_pending_seconds gauge\n",
            "crowdrelay_webhook_delivery_oldest_pending_seconds {}\n",
            "# HELP crowdrelay_push_delivery_pending Current queued or retry-wait push deliveries.\n",
            "# TYPE crowdrelay_push_delivery_pending gauge\n",
            "crowdrelay_push_delivery_pending {}\n",
            "# HELP crowdrelay_push_delivery_processing Current in-flight or acknowledgement-wait push deliveries.\n",
            "# TYPE crowdrelay_push_delivery_processing gauge\n",
            "crowdrelay_push_delivery_processing {}\n",
            "# HELP crowdrelay_push_delivery_dead Current real failed or ambiguous push deliveries, excluding intentional preference suppression.\n",
            "# TYPE crowdrelay_push_delivery_dead gauge\n",
            "crowdrelay_push_delivery_dead {}\n",
            "# HELP crowdrelay_push_delivery_suppressed Current fan push deliveries intentionally suppressed by category preference.\n",
            "# TYPE crowdrelay_push_delivery_suppressed gauge\n",
            "crowdrelay_push_delivery_suppressed {}\n",
            "# HELP crowdrelay_push_delivery_oldest_pending_seconds Age of the oldest ready push delivery.\n",
            "# TYPE crowdrelay_push_delivery_oldest_pending_seconds gauge\n",
            "crowdrelay_push_delivery_oldest_pending_seconds {}\n",
        ),
        http_snapshot.total,
        http_snapshot.errors_4xx,
        http_snapshot.errors_5xx,
        http_snapshot.latency_micros_sum as f64 / 1_000_000.0,
        http_snapshot.total,
        http_snapshot.le_50_ms,
        http_snapshot.le_100_ms,
        http_snapshot.le_250_ms,
        http_snapshot.le_500_ms,
        http_snapshot.le_1000_ms,
        http_snapshot.le_2500_ms,
        http_snapshot.le_5000_ms,
        http_snapshot.total,
        snapshot.queued,
        snapshot.persisted,
        snapshot.dropped,
        snapshot.persistence_failed,
        event_snapshot.queued,
        event_snapshot.persisted,
        event_snapshot.dropped,
        event_snapshot.persistence_failed,
        http_snapshot.legacy_area_claim_import_attempts,
        http_snapshot.legacy_area_wallet_import_attempts,
        http_snapshot.legacy_area_claim_imports,
        http_snapshot.legacy_area_wallet_imports,
        http_snapshot.legacy_static_staff_auth,
        ops_snapshot.outbox_pending,
        ops_snapshot.outbox_processing,
        ops_snapshot.outbox_dead,
        ops_snapshot.outbox_oldest_pending_seconds,
        ops_snapshot.delivery_pending,
        ops_snapshot.delivery_processing,
        ops_snapshot.delivery_dead,
        ops_snapshot.delivery_cancelled,
        ops_snapshot.delivery_oldest_pending_seconds,
        ops_snapshot.push_pending,
        ops_snapshot.push_processing,
        ops_snapshot.push_dead,
        ops_snapshot.push_suppressed,
        ops_snapshot.push_oldest_pending_seconds,
    );

    body.push_str(&http_metrics().route_prometheus());
    let pool = state.ticketing.pool();
    let pool_size = pool.size();
    let pool_idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX);
    let pool_in_use = pool_size.saturating_sub(pool_idle);
    let pool_max = pool.options().get_max_connections();
    let utilization = if pool_max == 0 {
        0.0
    } else {
        f64::from(pool_in_use) / f64::from(pool_max)
    };
    body.push_str(&format!(concat!(
        "# HELP crowdrelay_db_pool_size Current PostgreSQL pool size.\n# TYPE crowdrelay_db_pool_size gauge\n",
        "crowdrelay_db_pool_size {}\n",
        "# HELP crowdrelay_db_pool_idle Current idle PostgreSQL connections.\n# TYPE crowdrelay_db_pool_idle gauge\n",
        "crowdrelay_db_pool_idle {}\n",
        "# HELP crowdrelay_db_pool_in_use Current in-use PostgreSQL connections.\n# TYPE crowdrelay_db_pool_in_use gauge\n",
        "crowdrelay_db_pool_in_use {}\n",
        "# HELP crowdrelay_db_pool_max Configured maximum PostgreSQL connections.\n# TYPE crowdrelay_db_pool_max gauge\n",
        "crowdrelay_db_pool_max {}\n",
        "# HELP crowdrelay_db_pool_utilization_ratio PostgreSQL pool utilization against configured maximum.\n# TYPE crowdrelay_db_pool_utilization_ratio gauge\n",
        "crowdrelay_db_pool_utilization_ratio {:.6}\n"
    ), pool_size, pool_idle, pool_in_use, pool_max, utilization));

    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"),
            (CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn no_store_json<T>(status: StatusCode, body: T) -> impl IntoResponse
where
    T: Serialize,
{
    (status, [(CACHE_CONTROL, "no-store")], Json(body))
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[derive(Debug, Serialize)]
struct Problem {
    r#type: &'static str,
    title: &'static str,
    status: u16,
    detail: &'static str,
    #[serde(skip)]
    cache_control: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}

impl Problem {
    fn service_unavailable(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/dependency-unavailable",
            title: "Service temporarily unavailable",
            status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
            detail: "A required dependency is unavailable. Retry later.",
            cache_control: "no-store",
            request_id,
        }
    }

    fn bad_request(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/bad-request",
            title: "Bad request",
            status: StatusCode::BAD_REQUEST.as_u16(),
            detail: "The request could not be parsed or validated.",
            cache_control: "no-store",
            request_id,
        }
    }

    fn unauthorized(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/unauthorized",
            title: "Authentication required",
            status: StatusCode::UNAUTHORIZED.as_u16(),
            detail: "Valid authentication is required for this operation.",
            cache_control: "no-store",
            request_id,
        }
    }

    fn not_found(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/not-found",
            title: "Resource not found",
            status: StatusCode::NOT_FOUND.as_u16(),
            detail: "The requested resource does not exist or is inactive.",
            cache_control: "no-store",
            request_id,
        }
    }

    fn conflict(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/conflict",
            title: "Request conflicts with existing state",
            status: StatusCode::CONFLICT.as_u16(),
            detail: "The request cannot be applied to the current durable state.",
            cache_control: "no-store",
            request_id,
        }
    }

    fn unprocessable(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/policy-violation",
            title: "Request violates signup policy",
            status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
            detail: "The supplied values do not satisfy the signup policy.",
            cache_control: "no-store",
            request_id,
        }
    }

    fn payload_too_large(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/payload-too-large",
            title: "Request payload is too large",
            status: StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
            detail: "The request body exceeds the permitted size.",
            cache_control: "no-store",
            request_id,
        }
    }

    fn internal(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/internal",
            title: "Internal server error",
            status: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            detail: "The request could not be completed.",
            cache_control: "no-store",
            request_id,
        }
    }

    fn private(mut self) -> Self {
        self.cache_control = "private, no-store";
        self
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let cache_control = self.cache_control;
        (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [
                (CONTENT_TYPE, "application/problem+json"),
                (CACHE_CONTROL, cache_control),
            ],
            Json(self),
        )
            .into_response()
    }
}

#[cfg(test)]
include!("lib_tests.rs");
