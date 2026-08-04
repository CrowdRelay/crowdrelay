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

use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{
        HeaderMap, HeaderValue, Method, Request, StatusCode,
        header::{
            ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderName, IF_NONE_MATCH,
            InvalidHeaderValue,
        },
    },
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crowdrelay_infra::database;
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
mod concert_qr;
mod ecosystem;
mod event_copy;
mod events;
mod fan_lifecycle;
mod mobile_fan;
mod ops;
mod proofs;
mod referrals;
mod releases;
mod security;
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
pub use referrals::ReferralState;
pub use ticketing::TicketingState;

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const X_CROWDRELAY_CORRELATION_ID: HeaderName =
    HeaderName::from_static("x-crowdrelay-correlation-id");
const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
const MAX_PUBLIC_BODY_BYTES: usize = 16 * 1024;

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
    pub(crate) ops: OpsState,
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
        ops: OpsState,
    ) -> Self {
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
            ops,
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
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            ACCEPT,
            CONTENT_TYPE,
            AUTHORIZATION,
            IDEMPOTENCY_KEY,
            IF_NONE_MATCH,
            X_REQUEST_ID,
            X_CROWDRELAY_CORRELATION_ID,
        ])
        .expose_headers([CACHE_CONTROL, ETAG, X_REQUEST_ID]);

    let middleware = ServiceBuilder::new()
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

    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/health/live", get(live))
        .route("/v1/health/ready", get(ready))
        .route("/v1/go/{slug}", get(acquisition::redirect_smart_link))
        .route("/v1/fans", post(acquisition::signup_fan))
        .route("/v1/fans/access", post(fan_lifecycle::request_fan_access))
        .route("/v1/fans/confirm", post(fan_lifecycle::confirm_fan))
        .route("/v1/fans/unsubscribe", post(fan_lifecycle::unsubscribe_fan))
        .route("/v1/public/cities", get(acquisition::list_cities))
        .route("/v1/public/cities/requests", post(mobile_fan::request_city))
        .route("/v1/public/events", get(events::list_events))
        .route(
            "/v1/public/proofs/batches/{batch_id}",
            get(proofs::public_batch),
        )
        .route(
            "/v1/public/proofs/batches/{batch_id}/{source_kind}/{source_id}",
            get(proofs::public_inclusion),
        )
        .route(
            "/v1/public/proofs/draws/{draw_slug}",
            get(proofs::public_draw),
        )
        .route("/v1/public/events/{slug}", get(events::get_event))
        .route(
            "/v1/public/events/{slug}/tickets",
            get(ticketing::public_sale),
        )
        .route(
            "/v1/public/events/{slug}/ticket-orders",
            post(ticketing::reserve_order),
        )
        .route(
            "/v1/public/ticket-orders/{order_id}",
            get(ticketing::order_status),
        )
        .route(
            "/v1/public/ticket-orders/{order_id}/wallet",
            get(ticketing::order_wallet),
        )
        .route(
            "/v1/public/ticket-orders/{order_id}/delivery-requests",
            post(ticketing::request_delivery),
        )
        .route("/v1/public/events/{slug}/view", post(events::track_view))
        .route(
            "/v1/public/events/{slug}/ticket",
            get(events::ticket_redirect),
        )
        .route(
            "/v1/public/events/{slug}/listen",
            get(events::listen_redirect),
        )
        .route(
            "/v1/public/events/{slug}/calendar.ics",
            get(events::calendar),
        )
        .route("/v1/public/events/{slug}/share", post(events::track_share))
        .route(
            "/v1/events/{slug}/interest",
            post(events::register_interest),
        )
        .route("/v1/me/events", get(events::my_events))
        .route("/r/{code}", get(referrals::redirect_referral))
        .route("/v1/r/{code}", get(referrals::redirect_referral))
        .route("/v1/me/referral", get(referrals::referral_progress))
        .route(
            "/v1/commerce/coupons/redeem",
            post(referrals::redeem_coupon),
        )
        .route("/v1/staff/coupons/redeem", post(referrals::redeem_coupon))
        .route(
            "/v1/admin/events/{slug}/ticketing",
            get(ticketing::admin_overview).post(ticketing::configure_sale),
        )
        .route(
            "/v1/staff/events/{slug}/ticketing",
            get(ticketing::admin_overview),
        )
        .route(
            "/v1/internal/ticket-orders/{order_id}/stripe-checkout",
            post(ticketing::bind_stripe_checkout),
        )
        .route(
            "/v1/internal/ticket-orders/{order_id}/cancel",
            post(ticketing::cancel_order),
        )
        .route(
            "/v1/internal/ticket-orders/stripe-events",
            post(ticketing::stripe_event),
        )
        .route(
            "/v1/internal/events/{event_id}/copy-enrichment",
            post(event_copy::apply_event_copy),
        )
        .route(
            "/v1/internal/releases/announce",
            post(releases::announce_release),
        )
        .route(
            "/v1/internal/cities/{city_id}/geocode",
            post(mobile_fan::geocode_city),
        )
        .route(
            "/v1/internal/nearby-gigs/emit-due",
            post(mobile_fan::emit_due_nearby_gigs),
        )
        .route("/v1/internal/proofs/claim", post(proofs::internal_claim))
        .route(
            "/v1/internal/proofs/{batch_id}/confirm",
            post(proofs::internal_confirm),
        )
        .route(
            "/v1/internal/proofs/{batch_id}/fail",
            post(proofs::internal_fail),
        )
        .route(
            "/v1/internal/proofs/audit-batches",
            post(proofs::admin_create_audit_batch),
        )
        .route(
            "/v1/admin/accounting/profile",
            get(accounting::get_profile).post(accounting::configure_profile),
        )
        .route(
            "/v1/admin/accounting/ticket-sales/preview",
            get(accounting::preview_ticket_sales),
        )
        .route(
            "/v1/admin/accounting/ticket-sales/finalize",
            post(accounting::finalize_ticket_sales),
        )
        .route(
            "/v1/admin/accounting/invoice-requests",
            get(accounting::list_invoice_requests),
        )
        .route(
            "/v1/admin/accounting/documents/{document_id}/csv",
            get(accounting::download_accounting_csv),
        )
        .route("/v1/admin/ecosystem/overview", get(ecosystem::overview))
        .route("/v1/admin/ecosystem/flags", get(ecosystem::list_flags))
        .route(
            "/v1/admin/ecosystem/flags/{key}",
            post(ecosystem::update_flag),
        )
        .route("/v1/admin/ecosystem/reconcile", post(ecosystem::reconcile))
        .route(
            "/v1/admin/ecosystem/reconciliation",
            get(ecosystem::list_findings),
        )
        .route(
            "/v1/admin/ecosystem/checklists/{event_slug}",
            get(ecosystem::show_checklist),
        )
        .route(
            "/v1/admin/ecosystem/checklists/{event_slug}/{item_key}",
            post(ecosystem::update_checklist),
        )
        .route(
            "/v1/admin/ecosystem/checklists/emit-due",
            post(ecosystem::emit_due_checklists),
        )
        .route(
            "/v1/staff/ops/show-snapshot/{event_slug}",
            get(ecosystem::show_snapshot),
        )
        .route("/v1/admin/proofs/batches", get(proofs::admin_list_batches))
        .route(
            "/v1/admin/proofs/audit-batches",
            post(proofs::admin_create_audit_batch),
        )
        .route("/v1/admin/signal/overview", get(ops::signal_overview))
        .route("/v1/admin/ops/summary", get(ops::summary))
        .route("/v1/admin/ops/outbox", get(ops::list_outbox))
        .route("/v1/admin/ops/deliveries", get(ops::list_deliveries))
        .route(
            "/v1/admin/ops/deliveries/{delivery_id}",
            get(ops::delivery_details),
        )
        .route(
            "/v1/admin/ops/outbox/{event_id}/retry",
            post(ops::retry_outbox),
        )
        .route(
            "/v1/admin/ops/deliveries/{delivery_id}/retry",
            post(ops::retry_delivery),
        )
        .route("/v1/admin/admission/passes", post(admission::issue_pass))
        .route(
            "/v1/admin/event-qr/campaigns",
            get(concert_qr::list_campaigns).post(concert_qr::create_campaign),
        )
        .route("/v1/admin/event-qr/overview", get(concert_qr::overview))
        .route(
            "/v1/admin/event-qr/campaigns/{campaign_id}/revoke",
            post(concert_qr::revoke_campaign),
        )
        .route(
            "/v1/staff/event-qr/campaigns",
            get(concert_qr::list_campaigns).post(concert_qr::create_campaign),
        )
        .route("/v1/staff/event-qr/overview", get(concert_qr::overview))
        .route(
            "/v1/staff/event-qr/campaigns/{campaign_id}/revoke",
            post(concert_qr::revoke_campaign),
        )
        .route("/v1/events/{slug}/check-in", post(concert_qr::check_in))
        .route(
            "/v1/admin/admission/passes/{public_reference}/revoke",
            post(admission::revoke_pass),
        )
        .route("/v1/passes/claim", post(admission::claim_pass))
        .route("/v1/me/pass", get(admission::my_pass))
        .route("/v1/me/pass/qr", get(admission::my_pass_qr))
        .route("/v1/staff/admission/redeem", post(admission::redeem_pass))
        .layer(DefaultBodyLimit::max(MAX_PUBLIC_BODY_BYTES))
        .with_state(state.clone())
        .layer(from_fn_with_state(state, enforce_privileged_namespace))
        .layer(middleware)
}

async fn enforce_privileged_namespace(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let authorized = if path.starts_with("/v1/admin/") {
        state.ticketing.admin_authorized(request.headers())
    } else if path.starts_with("/v1/staff/") {
        state.ticketing.operator_authorized(request.headers())
    } else if path.starts_with("/v1/commerce/") || path.starts_with("/v1/internal/") {
        state.ticketing.commerce_authorized(request.headers())
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
        || path.starts_with("/v1/commerce/");
    let authorized = if path.starts_with("/v1/admin/") {
        state.ticketing.admin_authorized(request.headers())
    } else if path.starts_with("/v1/staff/") {
        state.ticketing.operator_authorized(request.headers())
    } else if path.starts_with("/v1/internal/") || path.starts_with("/v1/commerce/") {
        state.ticketing.commerce_authorized(request.headers())
    } else {
        false
    };
    if privileged && authorized {
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
    let snapshot = state.acquisition.click_metrics_snapshot();
    let event_snapshot = state.events.metrics_snapshot();
    let ops_snapshot = state.ops.metrics_snapshot().await.unwrap_or_default();
    let body = format!(
        concat!(
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
            "# HELP crowdrelay_webhook_delivery_oldest_pending_seconds Age of the oldest ready pending webhook delivery.\n",
            "# TYPE crowdrelay_webhook_delivery_oldest_pending_seconds gauge\n",
            "crowdrelay_webhook_delivery_oldest_pending_seconds {}\n",
        ),
        snapshot.queued,
        snapshot.persisted,
        snapshot.dropped,
        snapshot.persistence_failed,
        event_snapshot.queued,
        event_snapshot.persisted,
        event_snapshot.dropped,
        event_snapshot.persistence_failed,
        ops_snapshot.outbox_pending,
        ops_snapshot.outbox_processing,
        ops_snapshot.outbox_dead,
        ops_snapshot.outbox_oldest_pending_seconds,
        ops_snapshot.delivery_pending,
        ops_snapshot.delivery_processing,
        ops_snapshot.delivery_dead,
        ops_snapshot.delivery_oldest_pending_seconds,
    );

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
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{
            Request, StatusCode,
            header::{
                AUTHORIZATION, CONTENT_TYPE, COOKIE, ETAG, IF_NONE_MATCH, LOCATION, REFERER,
                SET_COOKIE,
            },
        },
    };
    use crowdrelay_application::{
        AcquisitionRepository, AdmissionRepository, ClaimAdmissionPass, ConfirmFan,
        ConfirmFanCommand, EventCache, EventRepository, FanLifecycleRepository, IssueAdmissionPass,
        ListCities, ListFanEventInterests, LoadAdmissionPass, LoadReferralProgress,
        RedeemAdmissionPass, RedeemCoupon, RedeemCouponCommand, RedirectCache, ReferralRepository,
        RegisterEventInterest, RegisterEventInterestCommand, RepositoryError, ResolveReferralCode,
        RevokeAdmissionPass, SignupFan, SignupFanCommand, UnsubscribeFan,
    };
    use crowdrelay_domain::{
        AdmissionPassClaimed, AdmissionPassIssued, AdmissionPassView, AdmissionRedemptionResult,
        CampaignId, CityId, CitySignal, CitySlug, ClickEvent, CountryCode, CouponRedemptionResult,
        CouponStatus, DestinationUrl, EventAction, EventInterestResult, FanActionToken,
        FanConfirmationResult, FanEventInterest, FanId, FanSessionToken, FanSignupResult,
        FanStatus, FanUnsubscribeResult, PassSessionToken, PublicEvent, ReferralCode,
        ReferralProgress, ResolvedSmartLink, SmartLinkId, SmartLinkSlug, WorkspaceId,
        WorkspaceSlug,
    };
    use serde_json::Value;
    use sha2::Digest;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;
    use url::Url;

    use crate::{AdmissionStateArgs, acquisition};

    use super::{
        AcquisitionState, AdmissionState, AppState, ClickSubmitter, ConcertQrState,
        EventActionMetricsSnapshot, EventState, FanLifecycleState, HttpConfig, OpsState,
        ReferralState, TicketingState, X_REQUEST_ID, router,
    };

    struct TestRepository {
        signup_result: Result<FanSignupResult, RepositoryError>,
        cities_result: Result<Vec<CitySignal>, RepositoryError>,
        signup_commands: Mutex<Vec<SignupFanCommand>>,
    }

    impl TestRepository {
        fn unavailable() -> Self {
            Self {
                signup_result: Err(RepositoryError::Unavailable),
                cities_result: Err(RepositoryError::Unavailable),
                signup_commands: Mutex::new(Vec::new()),
            }
        }

        fn happy() -> Result<Self, Box<dyn std::error::Error>> {
            Ok(Self {
                signup_result: Ok(FanSignupResult {
                    fan_id: FanId::new(),
                    status: FanStatus::Active,
                    referral_code: Some(ReferralCode::parse("Fan_Code-123")?),
                    fan_session_token: Some(FanSessionToken::parse(
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    )?),
                    confirmation_required: false,
                    created: true,
                    email_kind: None,
                    email_queued: false,
                    retry_after_seconds: None,
                }),
                cities_result: Ok(vec![CitySignal::new(
                    CityId::new(),
                    CitySlug::parse("wroclaw")?,
                    "Wrocław",
                    CountryCode::parse("PL")?,
                    42,
                )?]),
                signup_commands: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AcquisitionRepository for TestRepository {
        async fn resolve_workspace(
            &self,
            _slug: &WorkspaceSlug,
        ) -> Result<Option<WorkspaceId>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn persist_click_batch(&self, _clicks: &[ClickEvent]) -> Result<(), RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn persist_fan_signup(
            &self,
            command: &SignupFanCommand,
        ) -> Result<FanSignupResult, RepositoryError> {
            self.signup_commands
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(command.clone());
            self.signup_result.clone()
        }

        async fn list_city_signals(
            &self,
            _workspace_id: WorkspaceId,
            _limit: u32,
        ) -> Result<Vec<CitySignal>, RepositoryError> {
            self.cities_result.clone()
        }
    }

    struct TestReferralRepository;

    #[async_trait]
    impl ReferralRepository for TestReferralRepository {
        async fn referral_code_is_active(
            &self,
            _workspace_id: WorkspaceId,
            _code: &ReferralCode,
        ) -> Result<bool, RepositoryError> {
            Ok(true)
        }

        async fn load_referral_progress(
            &self,
            _workspace_id: WorkspaceId,
            _session_token: &FanSessionToken,
        ) -> Result<ReferralProgress, RepositoryError> {
            Ok(ReferralProgress {
                referral_code: ReferralCode::parse("Fan_Code-123")
                    .map_err(|_| RepositoryError::Unavailable)?,
                qualified_referrals: 3,
                pending_referrals: 0,
                next_reward_threshold: Some(5),
                draw_entries: Vec::new(),
                coupons: Vec::new(),
                physical_rewards: Vec::new(),
            })
        }

        async fn redeem_coupon(
            &self,
            _command: &RedeemCouponCommand,
        ) -> Result<CouponRedemptionResult, RepositoryError> {
            Ok(CouponRedemptionResult {
                coupon_id: crowdrelay_domain::MerchCouponId::new(),
                reward_grant_id: crowdrelay_domain::RewardGrantId::new(),
                status: CouponStatus::Redeemed,
                used_count: 1,
                max_uses: 1,
                redeemed_at: time::OffsetDateTime::UNIX_EPOCH,
            })
        }
    }

    struct TestEventRepository;

    #[async_trait]
    impl EventRepository for TestEventRepository {
        async fn load_published_events(&self) -> Result<Vec<PublicEvent>, RepositoryError> {
            Ok(Vec::new())
        }

        async fn persist_event_action(
            &self,
            _actions: &[EventAction],
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn register_interest(
            &self,
            _command: &RegisterEventInterestCommand,
        ) -> Result<EventInterestResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn list_fan_interests(
            &self,
            _workspace_id: WorkspaceId,
            _session_token: &FanSessionToken,
            _limit: u32,
        ) -> Result<Vec<FanEventInterest>, RepositoryError> {
            Ok(Vec::new())
        }
    }

    struct TestAdmissionRepository;

    #[async_trait]
    impl AdmissionRepository for TestAdmissionRepository {
        async fn issue_pass(
            &self,
            _command: &crowdrelay_application::IssueAdmissionPassCommand,
        ) -> Result<AdmissionPassIssued, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn claim_pass(
            &self,
            _command: &crowdrelay_application::ClaimAdmissionPassCommand,
        ) -> Result<AdmissionPassClaimed, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn load_pass(
            &self,
            _workspace_id: WorkspaceId,
            _session: &PassSessionToken,
        ) -> Result<AdmissionPassView, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn redeem_pass(
            &self,
            _command: &crowdrelay_application::RedeemAdmissionPassCommand,
        ) -> Result<AdmissionRedemptionResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn revoke_pass(
            &self,
            _command: &crowdrelay_application::RevokeAdmissionPassCommand,
        ) -> Result<AdmissionPassView, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }
    }

    struct TestFanLifecycleRepository;

    #[async_trait]
    impl FanLifecycleRepository for TestFanLifecycleRepository {
        async fn confirm(
            &self,
            _command: &ConfirmFanCommand,
        ) -> Result<FanConfirmationResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn unsubscribe(
            &self,
            _workspace_id: WorkspaceId,
            _token: &FanActionToken,
        ) -> Result<FanUnsubscribeResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }
    }

    fn admission_state(workspace_id: WorkspaceId) -> AdmissionState {
        let repository: Arc<dyn AdmissionRepository> = Arc::new(TestAdmissionRepository);
        AdmissionState::new(AdmissionStateArgs {
            workspace_id,
            issue_pass: IssueAdmissionPass::new(Arc::clone(&repository)),
            claim_pass: ClaimAdmissionPass::new(Arc::clone(&repository)),
            load_pass: LoadAdmissionPass::new(Arc::clone(&repository)),
            redeem_pass: RedeemAdmissionPass::new(Arc::clone(&repository)),
            revoke_pass: RevokeAdmissionPass::new(repository),
            admin_api_key_sha256: None,
            staff_api_key_sha256: None,
            qr_signing_key: None,
            qr_ttl: Duration::from_secs(30),
            secure_cookies: false,
        })
    }

    fn fan_lifecycle_state(
        workspace_id: WorkspaceId,
    ) -> Result<FanLifecycleState, Box<dyn std::error::Error>> {
        let repository: Arc<dyn FanLifecycleRepository> = Arc::new(TestFanLifecycleRepository);
        Ok(FanLifecycleState::new(
            workspace_id,
            ConfirmFan::new(Arc::clone(&repository)),
            UnsubscribeFan::new(repository),
            Url::parse("http://localhost:4321")?,
            false,
        ))
    }

    fn event_state(workspace_id: WorkspaceId) -> EventState {
        let repository: Arc<dyn EventRepository> = Arc::new(TestEventRepository);
        EventState::new(
            workspace_id,
            Arc::new(EventCache::new()),
            RegisterEventInterest::new(Arc::clone(&repository)),
            ListFanEventInterests::new(repository),
            Arc::new(|_action| {}),
            Arc::new(EventActionMetricsSnapshot::default),
        )
    }

    fn referral_state(
        workspace_id: WorkspaceId,
    ) -> Result<ReferralState, Box<dyn std::error::Error>> {
        let repository: Arc<dyn ReferralRepository> = Arc::new(TestReferralRepository);
        Ok(ReferralState::new(
            workspace_id,
            ResolveReferralCode::new(Arc::clone(&repository)),
            LoadReferralProgress::new(Arc::clone(&repository)),
            RedeemCoupon::new(repository),
            Url::parse("http://localhost:4321")?,
            false,
            Some(sha2::Sha256::digest(b"test-commerce-api-key-1234567890").into()),
            Some(sha2::Sha256::digest(b"test-staff-api-key-123456789012").into()),
            Some(sha2::Sha256::digest(b"test-admin-api-key-123456789012").into()),
        ))
    }

    fn acquisition_state(
        repository: Arc<dyn AcquisitionRepository>,
        workspace_id: WorkspaceId,
        redirect_cache: Arc<RedirectCache>,
        click_submitter: ClickSubmitter,
    ) -> Result<AcquisitionState, Box<dyn std::error::Error>> {
        Ok(AcquisitionState::new(acquisition::AcquisitionStateArgs {
            workspace_id,
            redirect_cache,
            signup_fan: SignupFan::new(Arc::clone(&repository)),
            list_cities: ListCities::new(repository),
            click_submitter,
            click_metrics_reader: Arc::new(super::ClickMetricsSnapshot::default),
            public_site_base_url: Url::parse("http://localhost:4321")?,
            secure_cookies: false,
        }))
    }

    fn state_with(
        repository: Arc<dyn AcquisitionRepository>,
        workspace_id: WorkspaceId,
        redirect_cache: Arc<RedirectCache>,
        click_submitter: ClickSubmitter,
    ) -> Result<AppState, Box<dyn std::error::Error>> {
        let database = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(50))
            .connect_lazy("postgres://crowdrelay:crowdrelay@127.0.0.1:1/crowdrelay")?;

        let concert_qr = ConcertQrState::new(workspace_id, database.clone(), None, None, None);
        let ticketing = TicketingState::new(
            workspace_id,
            database.clone(),
            Duration::from_millis(50),
            Duration::from_millis(50),
            Some(sha2::Sha256::digest(b"test-admin-api-key-123456789012").into()),
            Some(sha2::Sha256::digest(b"test-staff-api-key-123456789012").into()),
            Some(sha2::Sha256::digest(b"test-commerce-api-key-1234567890").into()),
            Some([7_u8; 32]),
        );
        let ops = OpsState::new(workspace_id, database.clone(), Duration::from_millis(50));
        Ok(AppState::new(
            database,
            Duration::from_millis(50),
            acquisition_state(repository, workspace_id, redirect_cache, click_submitter)?,
            referral_state(workspace_id)?,
            event_state(workspace_id),
            admission_state(workspace_id),
            concert_qr,
            fan_lifecycle_state(workspace_id)?,
            ticketing,
            ops,
        ))
    }

    fn unavailable_state() -> Result<AppState, Box<dyn std::error::Error>> {
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::unavailable());
        state_with(
            repository,
            WorkspaceId::new(),
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )
    }

    fn test_router() -> Result<axum::Router, Box<dyn std::error::Error>> {
        Ok(router(
            unavailable_state()?,
            HttpConfig::new(["http://localhost:4321".to_owned()])?,
        ))
    }

    fn test_router_with_state(state: AppState) -> Result<axum::Router, Box<dyn std::error::Error>> {
        Ok(router(
            state,
            HttpConfig::new(["http://localhost:4321".to_owned()])?,
        ))
    }

    #[tokio::test]
    async fn liveness_does_not_depend_on_database() -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(Request::builder().uri("/health/live").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(response.headers().contains_key(&X_REQUEST_ID));
        Ok(())
    }

    #[tokio::test]
    async fn versioned_liveness_contract_is_available() -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .uri("/v1/health/live")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn prometheus_endpoint_exposes_bounded_click_counters()
    -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(
            response.headers()[CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let body = to_bytes(response.into_body(), 16 * 1024).await?;
        let body = std::str::from_utf8(&body)?;
        assert!(body.contains("crowdrelay_click_events_dropped_total 0"));
        assert!(body.contains("crowdrelay_click_events_persistence_failed_total 0"));
        Ok(())
    }

    #[tokio::test]
    async fn replaces_client_supplied_request_id() -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .uri("/v1/health/live")
                    .header(&X_REQUEST_ID, "client-controlled-id")
                    .body(Body::empty())?,
            )
            .await?;

        let request_id = response.headers()[&X_REQUEST_ID].to_str()?;
        assert_ne!(request_id, "client-controlled-id");
        assert_eq!(request_id.len(), 36);
        Ok(())
    }

    #[tokio::test]
    async fn cors_allows_credentials_only_for_configured_origin()
    -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/v1/health/live")
                    .header("origin", "http://localhost:4321")
                    .header("access-control-request-method", "GET")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "http://localhost:4321"
        );
        assert_eq!(
            response.headers()["access-control-allow-credentials"],
            "true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn readiness_returns_problem_details_when_database_is_down()
    -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .uri("/v1/health/ready")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");

        let response_request_id = response.headers()[&X_REQUEST_ID].to_str()?.to_owned();
        let body = to_bytes(response.into_body(), 16 * 1024).await?;
        let problem: Value = serde_json::from_slice(&body)?;

        assert_eq!(problem["status"], 503);
        assert_eq!(problem["request_id"], response_request_id);
        Ok(())
    }

    #[tokio::test]
    async fn redirect_uses_only_the_cache_and_enqueues_anonymous_click()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let campaign_id = CampaignId::new();
        let link = ResolvedSmartLink::new(
            SmartLinkId::new(),
            workspace_id,
            Some(campaign_id),
            SmartLinkSlug::parse("tour-2026")?,
            DestinationUrl::parse("https://virya.music/join")?,
            1,
        )?;
        let cache = Arc::new(RedirectCache::new());
        cache.replace([link.clone()])?;
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let click_capture = Arc::clone(&clicks);
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::unavailable());
        let app = test_router_with_state(state_with(
            repository,
            workspace_id,
            cache,
            Arc::new(move |event| {
                click_capture
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event)
            }),
        )?)?;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/go/tour-2026")
                    .header(REFERER, "https://social.example/post/123")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers()[LOCATION], "https://virya.music/join");
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        let cookie = response.headers()[SET_COOKIE].to_str()?;
        assert!(cookie.contains("crowdrelay_attribution="));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(!cookie.contains("; Secure"));

        let clicks = clicks.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(clicks.len(), 1);
        assert_eq!(clicks[0].smart_link_id(), link.id());
        assert_eq!(clicks[0].campaign_id(), Some(campaign_id));
        assert_eq!(clicks[0].referrer_host(), Some("social.example"));
        assert!(clicks[0].visitor_id().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn fan_signup_is_private_and_propagates_server_request_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(TestRepository::happy()?);
        let repository_port: Arc<dyn AcquisitionRepository> = repository.clone();
        let workspace_id = WorkspaceId::new();
        let visitor_id = crowdrelay_domain::VisitorId::new();
        let app = test_router_with_state(state_with(
            repository_port,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "signup-test-0001")
                    .header(&X_REQUEST_ID, "must-be-replaced")
                    .header(COOKIE, format!("crowdrelay_attribution={visitor_id}"))
                    .body(Body::from(
                        r#"{"email":"Fan@Example.COM","display_name":"Ada","city_slug":"wroclaw","locale":"pl-PL","campaign_id":null,"consent":{"marketing":true,"policy_version":"privacy-v1"}}"#,
                    ))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        let fan_cookie = response.headers()[SET_COOKIE].to_str()?;
        assert!(fan_cookie.contains("crowdrelay_fan="));
        assert!(fan_cookie.contains("HttpOnly"));
        assert!(fan_cookie.contains("SameSite=Lax"));
        let response_request_id = response.headers()[&X_REQUEST_ID].to_str()?.to_owned();
        assert_ne!(response_request_id, "must-be-replaced");
        let body = to_bytes(response.into_body(), 16 * 1024).await?;
        let body: Value = serde_json::from_slice(&body)?;
        assert_eq!(body["status"], "active");
        assert_eq!(body["referral_url"], "http://localhost:4321/r/Fan_Code-123");

        let commands = repository
            .signup_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].request_id().as_str(), response_request_id);
        assert_eq!(commands[0].signup().workspace_id(), workspace_id);
        assert_eq!(commands[0].signup().email().as_str(), "fan@example.com");
        assert_eq!(commands[0].signup().visitor_id(), Some(visitor_id));
        Ok(())
    }

    #[tokio::test]
    async fn referral_cookie_is_used_when_signup_body_has_no_code()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(TestRepository::happy()?);
        let repository_port: Arc<dyn AcquisitionRepository> = repository.clone();
        let app = test_router_with_state(state_with(
            repository_port,
            WorkspaceId::new(),
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "signup-referral-cookie-0001")
                    .header(COOKIE, "crowdrelay_referral=Referrer_Code-123")
                    .body(Body::from(
                        r#"{"email":"cookie@example.com","city_slug":"wroclaw","consent":{"marketing":true,"policy_version":"privacy-v1"}}"#,
                    ))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::CREATED);
        let commands = repository
            .signup_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            commands[0]
                .signup()
                .claimed_referral_code()
                .map(ReferralCode::as_str),
            Some("Referrer_Code-123")
        );
        Ok(())
    }

    #[tokio::test]
    async fn privileged_namespaces_reject_cross_role_tokens()
    -> Result<(), Box<dyn std::error::Error>> {
        const ADMIN_KEY: &str = "test-admin-api-key-123456789012";
        const STAFF_KEY: &str = "test-staff-api-key-123456789012";
        const COMMERCE_KEY: &str = "test-commerce-api-key-1234567890";

        let app = test_router()?;
        for (uri, token) in [
            ("/v1/admin/events/test-show/ticketing", STAFF_KEY),
            ("/v1/admin/ops/summary", STAFF_KEY),
            ("/v1/staff/events/test-show/ticketing", COMMERCE_KEY),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let internal_with_admin = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/internal/ticket-orders/stripe-events")
                    .header(AUTHORIZATION, format!("Bearer {ADMIN_KEY}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))?,
            )
            .await?;
        assert_eq!(internal_with_admin.status(), StatusCode::UNAUTHORIZED);

        for (uri, token) in [
            ("/v1/admin/events/test-show/ticketing", ADMIN_KEY),
            ("/v1/admin/ops/summary", ADMIN_KEY),
            ("/v1/staff/events/test-show/ticketing", STAFF_KEY),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())?,
                )
                .await?;
            assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let internal_with_commerce = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/internal/ticket-orders/stripe-events")
                    .header(AUTHORIZATION, format!("Bearer {COMMERCE_KEY}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))?,
            )
            .await?;
        assert_ne!(internal_with_commerce.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn referral_redirect_progress_and_redemption_routes_are_private()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::happy()?);
        let app = test_router_with_state(state_with(
            repository,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;

        let redirect = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/r/Fan_Code-123")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(redirect.status(), StatusCode::FOUND);
        assert_eq!(redirect.headers()[LOCATION], "http://localhost:4321/join");
        let referral_cookie = redirect.headers()[SET_COOKIE].to_str()?;
        assert!(referral_cookie.contains("crowdrelay_referral=Fan_Code-123"));
        assert_eq!(redirect.headers()["cache-control"], "private, no-store");

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/me/referral")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let session = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let progress = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/me/referral")
                    .header(COOKIE, format!("crowdrelay_fan={session}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(progress.status(), StatusCode::OK);
        assert_eq!(progress.headers()["cache-control"], "private, no-store");

        let unauthorized_redeem = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/commerce/coupons/redeem")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "coupon-redeem-test-0001")
                    .body(Body::from(
                        r#"{"code":"VIRYA-ABC12345","order_reference":"order-1"}"#,
                    ))?,
            )
            .await?;
        assert_eq!(unauthorized_redeem.status(), StatusCode::UNAUTHORIZED);

        let redeemed = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/commerce/coupons/redeem")
                    .header(CONTENT_TYPE, "application/json")
                    .header("authorization", "Bearer test-commerce-api-key-1234567890")
                    .header("idempotency-key", "coupon-redeem-test-0001")
                    .body(Body::from(
                        r#"{"code":"VIRYA-ABC12345","order_reference":"order-1"}"#,
                    ))?,
            )
            .await?;
        assert_eq!(redeemed.status(), StatusCode::OK);
        assert_eq!(redeemed.headers()["cache-control"], "private, no-store");
        Ok(())
    }

    #[tokio::test]
    async fn fan_signup_requires_idempotency_and_explicit_consent()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(TestRepository::happy()?);
        let repository_port: Arc<dyn AcquisitionRepository> = repository.clone();
        let workspace_id = WorkspaceId::new();
        let app = test_router_with_state(state_with(
            repository_port,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;
        let body = r#"{"email":"fan@example.com","city_slug":"wroclaw","consent":{"marketing":false,"policy_version":"privacy-v1"}}"#;

        let missing_key = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(missing_key.status(), StatusCode::BAD_REQUEST);

        let refused_consent = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "signup-test-0002")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(refused_consent.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            repository
                .signup_commands
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn fan_signup_rejects_oversized_bodies_with_problem_details()
    -> Result<(), Box<dyn std::error::Error>> {
        let payload = format!(
            r#"{{"email":"fan@example.com","display_name":"{}","city_slug":"wroclaw","consent":{{"marketing":true,"policy_version":"privacy-v1"}}}}"#,
            "x".repeat(super::MAX_PUBLIC_BODY_BYTES)
        );
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "signup-test-large")
                    .body(Body::from(payload))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        Ok(())
    }

    #[tokio::test]
    async fn public_cities_support_strong_etag_revalidation()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::happy()?);
        let app = test_router_with_state(state_with(
            repository,
            WorkspaceId::new(),
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/public/cities?limit=20")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers()["cache-control"],
            "public, max-age=60, stale-while-revalidate=600, stale-if-error=86400"
        );
        let etag = first.headers()[ETAG].clone();
        let body = to_bytes(first.into_body(), 16 * 1024).await?;
        let body: Value = serde_json::from_slice(&body)?;
        assert_eq!(body["items"][0]["slug"], "wroclaw");
        assert_eq!(body["items"][0]["fan_count"], 42);

        let revalidated = app
            .oneshot(
                Request::builder()
                    .uri("/v1/public/cities?limit=20")
                    .header(IF_NONE_MATCH, etag)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
        assert!(revalidated.headers().contains_key(ETAG));
        assert!(
            to_bytes(revalidated.into_body(), 16 * 1024)
                .await?
                .is_empty()
        );
        Ok(())
    }
}
