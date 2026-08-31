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
    extract::{DefaultBodyLimit, Extension, MatchedPath, State},
    http::{
        HeaderMap, HeaderValue, Method, Request, StatusCode,
        header::{
            ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderName, IF_NONE_MATCH,
            InvalidHeaderValue,
        },
    },
    middleware::{Next, from_fn, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use crowdrelay_infra::{
    area_admin::PostgresAreaAdminRepository, autopilot::PostgresAutopilotRepository,
    beacon_signal::PostgresBeaconReleaseRepository,
    commerce_inventory::PostgresCommerceInventoryRepository,
    concert_qr::PostgresConcertQrRepository, database, ecosystem::PostgresEcosystemRepository,
    sensitive_response::SensitiveResponseKey,
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
mod audience_graph;
mod autopilot;
mod beacon_signal;
mod commerce;
mod concert_qr;
mod connections_simple;
mod connections_tiktok;
mod control_plane;
mod ecosystem;
mod event_copy;
mod events;
mod fan_context;
mod fan_lifecycle;
mod fan_privacy;
mod fanbase;
mod http_metrics;
mod meta;
mod mobile_fan;
mod ops;
mod ops_routes;
mod ops_summary;
mod portfolio;
mod proofs;
mod push;
mod rate_limit;
mod referrals;
mod releases;
mod routing;
mod security;
pub use rate_limit::{RateLimitPolicy, RateLimiter};
mod staff_sessions;
mod synesthesia;
mod synesthesia_gate;
pub mod tenant;
mod tenant_settings_http;
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
const X_TRACE_ID: HeaderName = HeaderName::from_static("x-trace-id");
const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");
const X_CROWDRELAY_RELEASE: HeaderName = HeaderName::from_static("x-crowdrelay-release");
const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");
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
    ControlPlane,
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
    control_plane_api_key_sha256: Option<[u8; 32]>,
    previous_area_management_api_key_sha256: Option<[u8; 32]>,
    previous_control_plane_api_key_sha256: Option<[u8; 32]>,
    pub(crate) ops: OpsState,
    pub(crate) autopilot: PostgresAutopilotRepository,
    pub(crate) autopilot_runtime_enabled: bool,
    /// Built from the pool this state already owns, so callers do not grow
    /// another constructor argument for it.
    pub(crate) ecosystem: PostgresEcosystemRepository,
    /// Beacon release + signal repository (write port). Built from the pool.
    pub(crate) beacon_release: PostgresBeaconReleaseRepository,
    /// Commerce inventory repository (write port). Built from the pool.
    pub(crate) commerce_inventory: PostgresCommerceInventoryRepository,
    /// Concert QR repository (write port). Built from the pool.
    pub(crate) concert_qr_repo: PostgresConcertQrRepository,
    pub(crate) push: push::PushPublicState,
    pub(crate) tenant: tenant::TenantProfile,
    /// Encryption key for OAuth token storage (TikTok, future providers).
    pub(crate) response_encryption_key: SensitiveResponseKey,
    /// Shared HTTP client for outbound OAuth token exchanges.
    pub(crate) http_client: reqwest::Client,
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
        control_plane_api_key_sha256: Option<[u8; 32]>,
        previous_area_management_api_key_sha256: Option<[u8; 32]>,
        previous_control_plane_api_key_sha256: Option<[u8; 32]>,
        ops: OpsState,
        autopilot: PostgresAutopilotRepository,
        autopilot_runtime_enabled: bool,
        push: push::PushPublicState,
        tenant: tenant::TenantProfile,
        response_encryption_key: SensitiveResponseKey,
    ) -> Self {
        let ecosystem = PostgresEcosystemRepository::new(database.clone());
        let beacon_release = PostgresBeaconReleaseRepository::new(database.clone());
        let commerce_inventory = PostgresCommerceInventoryRepository::new(database.clone());
        let concert_qr_repo = PostgresConcertQrRepository::new(database.clone());
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
            control_plane_api_key_sha256,
            previous_area_management_api_key_sha256,
            previous_control_plane_api_key_sha256,
            ops,
            autopilot,
            autopilot_runtime_enabled,
            ecosystem,
            beacon_release,
            commerce_inventory,
            concert_qr_repo,
            push,
            tenant,
            response_encryption_key,
            http_client: reqwest::Client::new(),
        }
    }
}

/// HTTP-level configuration validated before the router starts.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// CORS origins allowed to make credentialed requests.
    pub allowed_origins: Vec<HeaderValue>,
    /// Edge rate limiting policy; `None` disables the limiter entirely.
    pub rate_limiter: Option<Arc<RateLimiter>>,
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

        Ok(Self {
            allowed_origins,
            rate_limiter: None,
        })
    }

    /// Attaches an edge rate limiter built from validated policy.
    #[must_use]
    pub fn with_rate_limit(mut self, limiter: Option<Arc<RateLimiter>>) -> Self {
        self.rate_limiter = limiter;
        self
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
            X_TRACE_ID,
        ])
        .expose_headers([
            CACHE_CONTROL,
            ETAG,
            X_REQUEST_ID,
            X_TRACE_ID,
            SERVER_TIMING,
            X_CROWDRELAY_RELEASE,
        ]);

    let middleware = ServiceBuilder::new()
        .layer(from_fn(measure_request))
        .layer(from_fn_with_state(state.clone(), normalize_request_id))
        .layer(SetRequestIdLayer::new(X_REQUEST_ID, MakeRequestUuid))
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
        .layer(PropagateRequestIdLayer::new(X_REQUEST_ID))
        .layer(cors)
        .layer(Extension(config.rate_limiter))
        .layer(from_fn(rate_limit::enforce_rate_limits));

    routing::application_routes(state.clone())
        .merge(area_admin::router(state.clone()))
        .merge(control_plane::router(state.clone()))
        .layer(from_fn_with_state(state.clone(), gate_signal_routes))
        .layer(from_fn_with_state(state, enforce_privileged_namespace))
        .layer(middleware)
}

fn is_area_management_path(path: &str) -> bool {
    path == "/v1/control-plane/area" || path.starts_with("/v1/control-plane/area/")
}

fn one_segment_after(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|tail| !tail.is_empty() && !tail.contains('/'))
}

/// One identifier segment between a fixed prefix and a fixed suffix, e.g.
/// `/v1/control-plane/autopilot/actions/{id}/approve`.
fn one_segment_with_suffix(path: &str, prefix: &str, suffix: &str) -> bool {
    path.strip_prefix(prefix)
        .and_then(|tail| tail.strip_suffix(suffix))
        .is_some_and(|segment| !segment.is_empty() && !segment.contains('/'))
}

fn is_control_plane_management_path(path: &str) -> bool {
    path.starts_with("/v1/control-plane/ops/")
        || matches!(
            path,
            "/v1/control-plane/ecosystem/overview"
                | "/v1/control-plane/ecosystem/findings"
                | "/v1/control-plane/ecosystem/reconcile"
                | "/v1/control-plane/ecosystem/flags"
                | "/v1/control-plane/autopilot/overview"
                | "/v1/control-plane/autopilot/growth"
                | "/v1/control-plane/autopilot/growth-metrics/coverage"
                | "/v1/control-plane/autopilot/growth-metrics/trends"
                | "/v1/control-plane/autopilot/objectives"
                | "/v1/control-plane/autopilot/posture"
                | "/v1/control-plane/autopilot/acquisition-channels"
                | "/v1/control-plane/autopilot/tour-economics"
                | "/v1/control-plane/autopilot/show-economics"
                | "/v1/control-plane/autopilot/chief-of-staff"
                | "/v1/control-plane/autopilot/outreach/candidates"
                | "/v1/control-plane/autopilot/booking-discovery/candidates"
                | "/v1/control-plane/autopilot/beacon-signal"
                | "/v1/control-plane/autopilot/beacon-signal/candidates"
                | "/v1/control-plane/autopilot/beacon-press-requests"
                | "/v1/control-plane/autopilot/beacon-press-assets"
                | "/v1/control-plane/autopilot/beacon-signal-engagements"
                | "/v1/control-plane/autopilot/beacon-coverage"
                | "/v1/control-plane/autopilot/beacon-network"
                | "/v1/control-plane/autopilot/beacon-release-campaigns"
                | "/v1/control-plane/autopilot/plays"
                | "/v1/control-plane/autopilot/scorecard"
                | "/v1/control-plane/autopilot/reply-triage"
                | "/v1/control-plane/autopilot/next-best-actions"
                | "/v1/control-plane/autopilot/learning-loop"
                | "/v1/control-plane/portfolio/overview"
                | "/v1/control-plane/portfolio/amplification"
                | "/v1/control-plane/tenant-settings"
                | "/v1/control-plane/fanbases"
                | "/v1/control-plane/fanbases/connections"
                | "/v1/control-plane/webhook-endpoints"
                | "/v1/control-plane/audience/overview"
                | "/v1/control-plane/audience/fans"
                | "/v1/control-plane/audience/segments"
        )
        || one_segment_after(path, "/v1/control-plane/ecosystem/flags/")
        || one_segment_after(path, "/v1/control-plane/autopilot/policies/")
        || one_segment_after(path, "/v1/control-plane/tenant-settings/")
        || one_segment_with_suffix(path, "/v1/control-plane/autopilot/actions/", "/approve")
        || one_segment_with_suffix(
            path,
            "/v1/control-plane/autopilot/decisions/",
            "/handled-externally",
        )
        || one_segment_with_suffix(path, "/v1/control-plane/autopilot/decisions/", "/evidence")
        || one_segment_with_suffix(
            path,
            "/v1/control-plane/portfolio/amplification/",
            "/decide",
        )
        || one_segment_with_suffix(path, "/v1/control-plane/fanbases/", "/ingest")
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
    } else if is_control_plane_management_path(path) {
        authorization == Some(PrivilegedAuthorization::ControlPlane)
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

/// Gates Signal (beacon) routes on the `signal_enabled` product flag.
/// When a tenant opts out of Signal, all `/v1/beacon/` endpoints return 404.
///
/// The flag is read per request rather than from the boot-time tenant profile:
/// `signal_enabled` is editable at runtime through `/v1/control-plane/tenant-settings`,
/// and a snapshot taken at startup would leave the endpoints serving until the
/// process was restarted. `TenantSettingsRepository` caches behind a
/// process-wide 60-second TTL, so this costs a memory read on the warm path,
/// and only beacon requests reach it at all.
async fn gate_signal_routes(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if request.uri().path().starts_with("/v1/beacon/") {
        let enabled = crowdrelay_infra::tenant_settings::TenantSettingsRepository::new(
            state.database.clone(),
        )
        .brand_settings(state.ops.workspace_id().into_uuid())
        .await
        .map_or(state.tenant.products.signal, |brand| brand.signal_enabled);
        if !enabled {
            return Problem::not_found(request_id(request.headers()))
                .private()
                .into_response();
        }
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
        response.headers_mut().insert(SERVER_TIMING, value);
    }
    if let Ok(value) = HeaderValue::from_str(meta::release_identity()) {
        response.headers_mut().insert(X_CROWDRELAY_RELEASE, value);
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
        || is_area_management_path(path)
        || is_control_plane_management_path(path);
    let authorization =
        if path.starts_with("/v1/admin/") && state.ticketing.admin_authorized(request.headers()) {
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
            && security::bearer_sha256_matches_either(
                request.headers(),
                state.area_management_api_key_sha256,
                state.previous_area_management_api_key_sha256,
            )
        {
            Some(PrivilegedAuthorization::AreaManagement)
        } else if is_control_plane_management_path(path)
            && security::bearer_sha256_matches_either(
                request.headers(),
                state.control_plane_api_key_sha256,
                state.previous_control_plane_api_key_sha256,
            )
        {
            Some(PrivilegedAuthorization::ControlPlane)
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
            request.headers_mut().insert(X_REQUEST_ID, correlation);
        }
    }
    request.headers_mut().remove(&X_CROWDRELAY_CORRELATION_ID);
    // Extract or generate a trace_id for end-to-end execution tracing.
    // The trace_id propagates through API → outbox → worker → agents →
    // executor → measurement, connecting every event in an action's
    // lifecycle. If the caller provides X-Trace-Id, we reuse it; otherwise
    // we generate a new UUID v7 (time-ordered for index locality).
    let trace_id = request
        .headers()
        .get(&X_TRACE_ID)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| uuid::Uuid::parse_str(value.trim()).ok())
        .unwrap_or_else(uuid::Uuid::now_v7);
    // Store in request extensions for handlers to access via Extension<Uuid>.
    request.extensions_mut().insert(trace_id);
    // Also set the header so downstream middleware and the response see it.
    if let Ok(value) = HeaderValue::from_str(&trace_id.to_string()) {
        request.headers_mut().insert(X_TRACE_ID, value);
    }
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
    let ops_snapshot = match state.ops.metrics_snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(error = ?error, "operational metrics snapshot unavailable");
            let body = concat!(
                "# HELP crowdrelay_ops_metrics_snapshot_available Whether database-backed operational metrics are available.\n",
                "# TYPE crowdrelay_ops_metrics_snapshot_available gauge\n",
                "crowdrelay_ops_metrics_snapshot_available 0\n",
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [
                    (CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"),
                    (CACHE_CONTROL, "no-store"),
                ],
                body,
            )
                .into_response();
        }
    };
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
            "# HELP crowdrelay_http_rate_limited_total Requests rejected by the edge rate limiter, by limit class.\n",
            "# TYPE crowdrelay_http_rate_limited_total counter\n",
            "crowdrelay_http_rate_limited_total{{class=\"public_auth\"}} {}\n",
            "crowdrelay_http_rate_limited_total{{class=\"privileged\"}} {}\n",
            "crowdrelay_http_rate_limited_total{{class=\"general\"}} {}\n",
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
        http_snapshot.rate_limited_public_auth,
        http_snapshot.rate_limited_privileged,
        http_snapshot.rate_limited_general,
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

    body.push_str(
        "# HELP crowdrelay_ops_metrics_snapshot_available Whether database-backed operational metrics are available.\n\
# TYPE crowdrelay_ops_metrics_snapshot_available gauge\n\
crowdrelay_ops_metrics_snapshot_available 1\n",
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
        "# HELP crowdrelay_db_pool_max Configured PostgreSQL connections.\n# TYPE crowdrelay_db_pool_max gauge\n",
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

pub(crate) fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Extracts the trace_id from the X-Trace-Id header. Returns None if no
/// trace_id was set. Used by the trace timeline endpoint and by handlers
/// that need to propagate the trace_id to downstream systems.
#[allow(dead_code)]
pub(crate) fn trace_id(headers: &HeaderMap) -> Option<uuid::Uuid> {
    headers
        .get(&X_TRACE_ID)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| uuid::Uuid::parse_str(value.trim()).ok())
}

pub(crate) fn record_rate_limited(class: &'static str) {
    http_metrics().record_rate_limited(class);
}

#[derive(Debug, Serialize)]
struct Problem {
    r#type: &'static str,
    title: &'static str,
    status: u16,
    detail: &'static str,
    #[serde(skip)]
    cache_control: &'static str,
    #[serde(skip)]
    retry_after_seconds: Option<u64>,
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
            retry_after_seconds: None,
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
            retry_after_seconds: None,
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
            retry_after_seconds: None,
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
            retry_after_seconds: None,
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
            retry_after_seconds: None,
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
            retry_after_seconds: None,
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
            retry_after_seconds: None,
            request_id,
        }
    }

    fn too_many_requests(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/rate-limited",
            title: "Too many requests",
            status: StatusCode::TOO_MANY_REQUESTS.as_u16(),
            detail: "The request exceeded the permitted rate. Retry after the indicated interval.",
            cache_control: "no-store",
            retry_after_seconds: Some(1),
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
            retry_after_seconds: None,
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
        let retry_after = self.retry_after_seconds;
        let mut response = (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [
                (CONTENT_TYPE, "application/problem+json"),
                (CACHE_CONTROL, cache_control),
            ],
            Json(self),
        )
            .into_response();
        if let Some(seconds) = retry_after
            && let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string())
        {
            response.headers_mut().insert(RETRY_AFTER, value);
        }
        response
    }
}

#[cfg(test)]
include!("lib_tests.rs");
