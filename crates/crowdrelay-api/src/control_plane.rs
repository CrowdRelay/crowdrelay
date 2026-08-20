//! Narrow server-to-server surface used by the multi-tenant Control Plane.
//!
//! This namespace deliberately reuses canonical admin handlers while exposing
//! only operational reads and bounded feature/autonomy mutations. It has its
//! own credential and must never grow into an alias for `/v1/admin`.

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};

const MAX_CONTROL_BODY_BYTES: usize = 8 * 1024;

pub(crate) fn router(state: crate::AppState) -> Router {
    Router::new()
        .route("/v1/control-plane/ops/summary", get(crate::ops::summary))
        .route(
            "/v1/control-plane/ops/deliveries/dead/clear",
            post(crate::ops::clear_dead_deliveries),
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
            "/v1/control-plane/autopilot/policies/{context}",
            post(crate::autopilot::set_authority),
        )
        .layer(DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES))
        .with_state(state)
}
