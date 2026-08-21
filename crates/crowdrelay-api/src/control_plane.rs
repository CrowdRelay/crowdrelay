//! Narrow server-to-server surface used by the multi-tenant Control Plane.
//!
//! This namespace deliberately reuses canonical admin handlers while exposing
//! only operational reads and bounded feature/autonomy mutations. It has its
//! own credential and must never grow into an alias for `/v1/admin`.

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::Request,
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post},
};

const MAX_CONTROL_BODY_BYTES: usize = 8 * 1024;

pub(crate) fn router(state: crate::AppState) -> Router {
    Router::new()
        .route("/v1/control-plane/ops/summary", get(crate::ops::summary))
        .route(
            "/v1/control-plane/ops/attention",
            get(crate::ops::attention),
        )
        .route("/v1/control-plane/ops/outbox", get(crate::ops::list_outbox))
        .route(
            "/v1/control-plane/ops/outbox/{event_id}/retry",
            post(crate::ops::retry_outbox),
        )
        .route(
            "/v1/control-plane/ops/deliveries",
            get(crate::ops::list_deliveries),
        )
        .route(
            "/v1/control-plane/ops/deliveries/dead/clear",
            post(crate::ops::clear_dead_deliveries),
        )
        .route(
            "/v1/control-plane/ops/deliveries/{delivery_id}",
            get(crate::ops::delivery_details),
        )
        .route(
            "/v1/control-plane/ops/deliveries/{delivery_id}/retry",
            post(crate::ops::retry_delivery),
        )
        .route(
            "/v1/control-plane/ops/operations/{request_id}",
            get(crate::ops::operation_timeline),
        )
        .route(
            "/v1/control-plane/ecosystem/overview",
            get(crate::ecosystem::overview),
        )
        .route(
            "/v1/control-plane/ecosystem/findings",
            get(crate::ecosystem::list_findings),
        )
        .route(
            "/v1/control-plane/ecosystem/reconcile",
            post(crate::ecosystem::reconcile),
        )
        .route(
            "/v1/control-plane/ecosystem/flags",
            get(crate::ecosystem::list_flags),
        )
        .route(
            "/v1/control-plane/ecosystem/flags/{key}",
            post(crate::ecosystem::update_flag),
        )
        .route(
            "/v1/control-plane/autopilot/overview",
            get(crate::autopilot::overview),
        )
        .route(
            "/v1/control-plane/autopilot/growth",
            get(crate::autopilot::growth),
        )
        .route(
            "/v1/control-plane/autopilot/policies/{context}",
            post(crate::autopilot::set_authority),
        )
        // Route-local authentication is intentional. The global middleware
        // still separates AREA and management credentials, but this guard
        // makes adding a route here fail closed even if the global path matcher
        // or the private tunnel allowlist has not been updated yet.
        .route_layer(from_fn_with_state(state.clone(), require_control_plane))
        .layer(DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES))
        .with_state(state)
}

async fn require_control_plane(
    State(state): State<crate::AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !crate::security::bearer_sha256_matches(
        request.headers(),
        state.control_plane_api_key_sha256,
    ) {
        return crate::Problem::unauthorized(crate::request_id(request.headers()))
            .private()
            .into_response();
    }
    next.run(request).await
}
