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
mod attestation;
mod audience;
mod audience_graph;
mod autopilot;
mod band_listing;
mod beacon_signal;
mod booking_agents;
mod commerce;
mod community_intelligence_routes;
mod concert_qr;
mod connections_gdrive;
mod connections_gmail;
mod connections_simple;
mod connections_tiktok;
mod content_engine;
mod control_plane;
mod ecosystem;
mod event_copy;
mod events;
mod fan_context;
mod fan_identity;
mod fan_lifecycle;
mod fan_privacy;
mod fanbase;
mod gdrive;
mod gig_planning;
mod http_metrics;
mod latarnik_http;
mod media;
mod meta;
mod mobile_fan;
mod night;
mod ops;
mod ops_routes;
mod ops_summary;
mod organization_settings_http;
mod portfolio;
mod proofs;
mod push;
mod rate_limit;
mod referrals;
mod releases;
mod roster_act_report;
mod roster_catalogue_rotation;
mod roster_counterparty_archive;
mod roster_overview;
mod roster_portfolio;
mod roster_release_calendar;
mod roster_source_roi;
mod roster_weekly_brief;
mod routing;
mod security;
mod signal_installations;
pub use rate_limit::{RateLimitPolicy, RateLimiter};
mod staff_sessions;
mod synesthesia;
mod synesthesia_gate;
mod team_approvals;
pub mod tenant;
mod tenant_settings_http;
mod ticket_qr;
mod ticketing;
mod workspace_secrets_http;

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
    /// Sealing keys for the workspace-secrets store — the tenant-configured
    /// credentials the control-plane secrets routes write and the internal
    /// credentials route opens.
    pub(crate) workspace_secrets_key: SensitiveResponseKey,
    pub(crate) previous_workspace_secrets_key: Option<SensitiveResponseKey>,
    /// Signing key for audience attestations. A document's signature is the
    /// only part a stranger can check against us rather than against itself.
    pub(crate) attestation_signing_key: crowdrelay_infra::attestation::AttestationSigningKey,
    /// Signing key for the mailed one-click approval links — same configured
    /// secret as the attestation key, its own domain separator.
    pub(crate) team_approval_key: crowdrelay_domain::team_approval_token::TeamApprovalKey,
    /// Shared HTTP client for outbound OAuth token exchanges.
    pub(crate) http_client: reqwest::Client,
    /// Provider verifiers for connection creation probes.
    pub(crate) provider_verifiers: crowdrelay_infra::provider_verification::ProviderVerifiers,
    /// The origin outside callers (Meta's crawler, a fan's mail client)
    /// actually reach — mints absolute `/v1/public/*` URLs such as uploaded
    /// media. `None` means the deployment never set it; handlers degrade to
    /// a relative path rather than guessing a host.
    pub(crate) public_api_origin: Option<url::Url>,
    /// Tenant-uploaded media store (`media_objects`). Built from the pool —
    /// the api-sql ratchet keeps the write out of the handlers.
    pub(crate) media: crowdrelay_infra::media::MediaRepository,
    /// The process-wide connection budget the operations page and every
    /// fanning control-plane read acquires from — see
    /// `ops::ControlPlaneReadBudget`. One page load firing nine endpoints at
    /// once used to take the entire pool; the budget turns that into queueing.
    pub(crate) read_budget: ops::ControlPlaneReadBudget,
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
        workspace_secrets_key: SensitiveResponseKey,
        previous_workspace_secrets_key: Option<SensitiveResponseKey>,
        attestation_signing_key: crowdrelay_infra::attestation::AttestationSigningKey,
        team_approval_key: crowdrelay_domain::team_approval_token::TeamApprovalKey,
        provider_verifiers: crowdrelay_infra::provider_verification::ProviderVerifiers,
        public_api_origin: Option<url::Url>,
    ) -> Self {
        let ecosystem = PostgresEcosystemRepository::new(database.clone());
        let beacon_release = PostgresBeaconReleaseRepository::new(database.clone());
        let commerce_inventory = PostgresCommerceInventoryRepository::new(database.clone());
        let concert_qr_repo = PostgresConcertQrRepository::new(database.clone());
        let area_admin = crowdrelay_application::AreaAdminService::new(Arc::new(
            PostgresAreaAdminRepository::new(database.clone()),
        ));
        let read_budget = ops::ControlPlaneReadBudget::new(&database);
        let media = crowdrelay_infra::media::MediaRepository::new(database.clone());
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
            workspace_secrets_key,
            previous_workspace_secrets_key,
            attestation_signing_key,
            team_approval_key,
            http_client: reqwest::Client::new(),
            provider_verifiers,
            public_api_origin,
            media,
            read_budget,
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

/// The Control Plane's own namespace. Every path under it requires the
/// derived ControlPlane bearer — the area sub-namespace alone takes the
/// narrower AreaManagement token instead.
///
/// This is a prefix, not a list, on purpose: the list that used to live here
/// had to be edited by hand for every new route, and a route missing from it
/// did not become unreachable — outside `control_plane::router`'s own auth
/// layer it answered *unauthenticated*. A prefix boundary cannot be
/// forgotten.
fn is_control_plane_management_path(path: &str) -> bool {
    (path == "/v1/control-plane" || path.starts_with("/v1/control-plane/"))
        && !is_area_management_path(path)
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
            // HELP and TYPE both name the family. Naming `_sum` attached the
            // metadata to a derived series that has no type of its own.
            "# HELP crowdrelay_http_request_duration_seconds Request wall time.\n",
            "# TYPE crowdrelay_http_request_duration_seconds histogram\n",
            "crowdrelay_http_request_duration_seconds_sum {:.6}\n",
            "crowdrelay_http_request_duration_seconds_count {}\n",
            "crowdrelay_http_request_duration_seconds_bucket{{le=\"0.05\"}} {}\n",
            "crowdrelay_http_request_duration_seconds_bucket{{le=\"0.10\"}} {}\n",
            "crowdrelay_http_request_duration_seconds_bucket{{le=\"0.25\"}} {}\n",
            "crowdrelay_http_request_duration_seconds_bucket{{le=\"0.50\"}} {}\n",
            "crowdrelay_http_request_duration_seconds_bucket{{le=\"1.00\"}} {}\n",
            "crowdrelay_http_request_duration_seconds_bucket{{le=\"2.50\"}} {}\n",
            "crowdrelay_http_request_duration_seconds_bucket{{le=\"5.00\"}} {}\n",
            "crowdrelay_http_request_duration_seconds_bucket{{le=\"+Inf\"}} {}\n",
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
            "# HELP crowdrelay_legacy_static_staff_auth_total Requests authenticated with the deprecated global staff bearer instead of a device session.\n",
            "# TYPE crowdrelay_legacy_static_staff_auth_total counter\n",
            "crowdrelay_legacy_static_staff_auth_total {}\n",
            "# HELP crowdrelay_http_rate_limited_total Requests rejected by the edge rate limiter, by limit class.\n",
            "# TYPE crowdrelay_http_rate_limited_total counter\n",
            "crowdrelay_http_rate_limited_total{{class=\"public_auth\"}} {}\n",
            "crowdrelay_http_rate_limited_total{{class=\"privileged\"}} {}\n",
            "crowdrelay_http_rate_limited_total{{class=\"general\"}} {}\n",
            "crowdrelay_http_rate_limited_total{{class=\"signup\"}} {}\n",
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
        http_snapshot.legacy_static_staff_auth,
        http_snapshot.rate_limited_public_auth,
        http_snapshot.rate_limited_privileged,
        http_snapshot.rate_limited_general,
        http_snapshot.rate_limited_signup,
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

    // Worker liveness. The worker serves no HTTP, so Prometheus cannot scrape
    // it directly and `up{job="crowdrelay-worker"}` does not exist. It renews
    // its leadership lease every 15 seconds, so the age of that lease is a
    // true heartbeat, and this process is already a scrape target.
    body.push_str(&format!(
        "# HELP crowdrelay_worker_lease_age_seconds Seconds since the worker last renewed its leadership lease.\n\
# TYPE crowdrelay_worker_lease_age_seconds gauge\n\
crowdrelay_worker_lease_age_seconds {}\n",
        ops_snapshot.worker_lease_age_seconds,
    ));

    // Brain health, beside queue health and for the same reason.
    //
    // Everything else here that can stall silently is already a gauge. The
    // brain was not, so "is it cycling, deciding, acting and learning" needed
    // the authenticated control plane to answer — and when a key was wrong,
    // a database shell on the host. That is a poor way to learn that the part
    // the product rests on has stopped.
    //
    // `seconds_since_cycle` is the one to alert on: the worker lease can be
    // fresh while the brain inside that same process has stopped cycling, and
    // no other signal here separates those two failures.
    body.push_str(&format!(
        "# HELP crowdrelay_brain_cycles_24h Autopilot cycles started in the last 24 hours.\n\
# TYPE crowdrelay_brain_cycles_24h gauge\n\
crowdrelay_brain_cycles_24h {}\n\
# HELP crowdrelay_brain_cycles_degraded_24h Cycles in the last 24 hours that did not succeed.\n\
# TYPE crowdrelay_brain_cycles_degraded_24h gauge\n\
crowdrelay_brain_cycles_degraded_24h {}\n\
# HELP crowdrelay_brain_seconds_since_cycle Seconds since the last autopilot cycle started.\n\
# TYPE crowdrelay_brain_seconds_since_cycle gauge\n\
crowdrelay_brain_seconds_since_cycle {}\n\
# HELP crowdrelay_brain_decisions_24h Autopilot decisions written in the last 24 hours.\n\
# TYPE crowdrelay_brain_decisions_24h gauge\n\
crowdrelay_brain_decisions_24h {}\n\
# HELP crowdrelay_brain_actions_24h Autopilot actions created in the last 24 hours.\n\
# TYPE crowdrelay_brain_actions_24h gauge\n\
crowdrelay_brain_actions_24h {}\n\
# HELP crowdrelay_brain_actions_failed_24h Autopilot actions created in the last 24 hours that failed.\n\
# TYPE crowdrelay_brain_actions_failed_24h gauge\n\
crowdrelay_brain_actions_failed_24h {}\n\
# HELP crowdrelay_brain_approvals_awaiting Autopilot asks waiting on a human approval, of any age.\n\
# TYPE crowdrelay_brain_approvals_awaiting gauge\n\
crowdrelay_brain_approvals_awaiting {}\n\
# HELP crowdrelay_brain_measurements_pending Scheduled measurements that have not resolved.\n\
# TYPE crowdrelay_brain_measurements_pending gauge\n\
crowdrelay_brain_measurements_pending {}\n\
# HELP crowdrelay_brain_measurements_resolved Measurements that resolved successfully.\n\
# TYPE crowdrelay_brain_measurements_resolved gauge\n\
crowdrelay_brain_measurements_resolved {}\n\
# HELP crowdrelay_brain_measurement_oldest_overdue_seconds Age of the oldest due-but-unresolved measurement; zero when none is overdue.\n\
# TYPE crowdrelay_brain_measurement_oldest_overdue_seconds gauge\n\
crowdrelay_brain_measurement_oldest_overdue_seconds {}\n\
# HELP crowdrelay_brain_agent_outcomes_processed_24h LLM worker outcomes accepted by the data-quality gate in the last 24 hours.\n\
# TYPE crowdrelay_brain_agent_outcomes_processed_24h gauge\n\
crowdrelay_brain_agent_outcomes_processed_24h {}\n\
# HELP crowdrelay_brain_agent_outcomes_rejected_24h LLM worker outcomes refused by the data-quality gate in the last 24 hours.\n\
# TYPE crowdrelay_brain_agent_outcomes_rejected_24h gauge\n\
crowdrelay_brain_agent_outcomes_rejected_24h {}\n\
# HELP crowdrelay_brain_communities_blocked_on_join Communities with a wanted post that nobody has joined, so the post cannot be dispatched.\n\
# TYPE crowdrelay_brain_communities_blocked_on_join gauge\n\
crowdrelay_brain_communities_blocked_on_join {}\n\
# HELP crowdrelay_brain_communities_joined Subreddits this workspace has joined and can post to.\n\
# TYPE crowdrelay_brain_communities_joined gauge\n\
crowdrelay_brain_communities_joined {}\n\
# HELP crowdrelay_brain_communities_rejected Subreddits in the terminal rejected state, which the join worker never retries.\n\
# TYPE crowdrelay_brain_communities_rejected gauge\n\
crowdrelay_brain_communities_rejected {}\n\
# HELP crowdrelay_brain_evidence_resolved Evidence rows that completed the full loop: predicted, dispatched, executed, measured, observed.\n\
# TYPE crowdrelay_brain_evidence_resolved gauge\n\
crowdrelay_brain_evidence_resolved {}\n\
# HELP crowdrelay_brain_seconds_since_evidence_resolved Seconds since the loop last closed; zero when it never has.\n\
# TYPE crowdrelay_brain_seconds_since_evidence_resolved gauge\n\
crowdrelay_brain_seconds_since_evidence_resolved {}\n\
# HELP crowdrelay_brain_seconds_since_publication Seconds since a community post last reached a platform; zero when none ever has.\n\
# TYPE crowdrelay_brain_seconds_since_publication gauge\n\
crowdrelay_brain_seconds_since_publication {}\n\
# HELP crowdrelay_brain_signal_installs Signal app installations recorded, whether or not they have identified themselves.\n\
# TYPE crowdrelay_brain_signal_installs gauge\n\
crowdrelay_brain_signal_installs {}\n\
# HELP crowdrelay_brain_signal_installs_identified Installations that have linked to a fan.\n\
# TYPE crowdrelay_brain_signal_installs_identified gauge\n\
crowdrelay_brain_signal_installs_identified {}\n\
# HELP crowdrelay_brain_signal_fans_push_enabled Fans reachable by push, the bottom of the Signal activation funnel.\n\
# TYPE crowdrelay_brain_signal_fans_push_enabled gauge\n\
crowdrelay_brain_signal_fans_push_enabled {}\n",
        ops_snapshot.brain_cycles_24h,
        ops_snapshot.brain_cycles_degraded_24h,
        ops_snapshot.brain_seconds_since_cycle,
        ops_snapshot.brain_decisions_24h,
        ops_snapshot.brain_actions_24h,
        ops_snapshot.brain_actions_failed_24h,
        ops_snapshot.brain_approvals_awaiting,
        ops_snapshot.brain_measurements_pending,
        ops_snapshot.brain_measurements_resolved,
        ops_snapshot.brain_measurement_oldest_overdue_seconds,
        ops_snapshot.brain_agent_outcomes_processed_24h,
        ops_snapshot.brain_agent_outcomes_rejected_24h,
        ops_snapshot.brain_communities_blocked_on_join,
        ops_snapshot.brain_communities_joined,
        ops_snapshot.brain_communities_rejected,
        ops_snapshot.brain_evidence_resolved,
        ops_snapshot.brain_seconds_since_evidence_resolved,
        ops_snapshot.brain_seconds_since_publication,
        ops_snapshot.brain_signal_installs,
        ops_snapshot.brain_signal_installs_identified,
        ops_snapshot.brain_signal_fans_push_enabled,
    ));

    // The fan-graph level is emitted only when an activation KPI row exists:
    // exporting 0 for "never measured" would let the Control Plane freeze a
    // baseline of zero that nobody counted, and the ninety-day guarantee
    // would start from a number that was never true.
    if let Some(fans) = ops_snapshot.brain_north_star_fans {
        body.push_str(&format!(
            concat!(
                "# HELP crowdrelay_brain_north_star_fans The fan graph's level (activated_fans_30d); the heartbeat forwards it so the Control Plane can freeze the activation baseline and answer the ninety-day guarantee as a query.\n",
                "# TYPE crowdrelay_brain_north_star_fans gauge\n",
                "crowdrelay_brain_north_star_fans {}\n"
            ),
            fans
        ));
    }

    match state.ops.growth_component_prometheus().await {
        Ok(block) => body.push_str(&block),
        Err(error) => {
            tracing::warn!(error = ?error, "growth component state unavailable");
        }
    }

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
    detail: std::borrow::Cow<'static, str>,
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
            detail: std::borrow::Cow::Borrowed(
                "A required dependency is unavailable. Retry later.",
            ),
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
            detail: std::borrow::Cow::Borrowed("The request could not be parsed or validated."),
            cache_control: "no-store",
            retry_after_seconds: None,
            request_id,
        }
    }

    /// A bad request that names the failing part — the generic detail
    /// tells the operator it failed, this one tells them which value did.
    fn bad_request_because(detail: &'static str, request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/bad-request",
            title: "Bad request",
            status: StatusCode::BAD_REQUEST.as_u16(),
            detail: std::borrow::Cow::Borrowed(detail),
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
            detail: std::borrow::Cow::Borrowed(
                "Valid authentication is required for this operation.",
            ),
            cache_control: "no-store",
            retry_after_seconds: None,
            request_id,
        }
    }

    /// The tenant has not opted into the capability the request needs.
    /// Distinct from `unauthorized` — the caller is authenticated fine; the
    /// tenant it acts for has the feature off.
    fn forbidden(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/forbidden",
            title: "Not permitted for this tenant",
            status: StatusCode::FORBIDDEN.as_u16(),
            detail: std::borrow::Cow::Borrowed(
                "The tenant has not enabled the capability this request needs.",
            ),
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
            detail: std::borrow::Cow::Borrowed(
                "The requested resource does not exist or is inactive.",
            ),
            cache_control: "no-store",
            retry_after_seconds: None,
            request_id,
        }
    }

    /// The thing existed once and its window closed — a mailed link past its
    /// expiry is gone, not missing.
    fn gone(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/gone",
            title: "Gone",
            status: StatusCode::GONE.as_u16(),
            detail: std::borrow::Cow::Borrowed("The resource existed but its window has closed."),
            cache_control: "no-store",
            retry_after_seconds: None,
            request_id,
        }
    }

    fn conflict(request_id: Option<String>) -> Self {
        Self::conflict_because(
            "The request cannot be applied to the current durable state.",
            request_id,
        )
    }

    /// A conflict that names its cause.
    ///
    /// The generic detail above is true of every 409 and actionable for none.
    /// Where the repository knows why — an action that already ran, an approval
    /// window that closed — that reason reaches the operator instead.
    fn conflict_because(detail: &'static str, request_id: Option<String>) -> Self {
        Self::conflict_owned(std::borrow::Cow::Borrowed(detail), request_id)
    }

    /// A conflict carrying a reason the domain wrote at runtime — a gate
    /// refusal the band reads, where the sentence is not known statically.
    fn conflict_owned(detail: std::borrow::Cow<'static, str>, request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/conflict",
            title: "Request conflicts with existing state",
            status: StatusCode::CONFLICT.as_u16(),
            detail,
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
            detail: std::borrow::Cow::Borrowed(
                "The supplied values do not satisfy the signup policy.",
            ),
            cache_control: "no-store",
            retry_after_seconds: None,
            request_id,
        }
    }

    /// A stored audience segment whose filter this build cannot read.
    ///
    /// Deliberately not a 503. Nothing is temporarily unavailable and a retry
    /// changes nothing: the row holds a field the filter type does not have, or
    /// a value it rejects, and it will hold the same one in a minute. The
    /// generic dependency-unavailable problem sent the console a sentence about
    /// the backend possibly "not supporting this segment", which is the one
    /// thing that was never true — the backend stores the segment and cannot
    /// parse it.
    fn segment_filter_unreadable(request_id: Option<String>) -> Self {
        Self {
            r#type: "https://crowdrelay.dev/problems/segment-filter-unreadable",
            title: "Segment filter cannot be read",
            status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
            detail: std::borrow::Cow::Borrowed(
                "This segment's stored filter carries a field this build does not know, or a \
                 value it rejects, so its audience cannot be counted. The server log names the \
                 field; the segment has to be rewritten before it can be previewed or sent to.",
            ),
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
            detail: std::borrow::Cow::Borrowed("The request body exceeds the permitted size."),
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
            detail: std::borrow::Cow::Borrowed(
                "The request exceeded the permitted rate. Retry after the indicated interval.",
            ),
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
            detail: std::borrow::Cow::Borrowed("The request could not be completed."),
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
