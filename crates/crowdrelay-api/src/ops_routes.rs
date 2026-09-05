use axum::{
    Router,
    routing::{get, post},
};

pub(crate) fn router() -> Router<crate::AppState> {
    Router::new()
        .route("/v1/admin/ops/summary", get(crate::ops::summary))
        .route(
            "/v1/admin/ops/operations/{request_id}",
            get(crate::ops::operation_timeline),
        )
        .route(
            "/v1/admin/ops/trace/{trace_id}",
            get(crate::ops::trace_timeline),
        )
        .route("/v1/admin/ops/actions", get(crate::ops::list_actions))
        .route("/v1/admin/ops/cycles", get(crate::ops::list_cycles))
        .route(
            "/v1/admin/ops/connections",
            get(crate::ops::list_connection_health),
        )
        .route(
            "/v1/admin/ops/actions/{action_id}",
            get(crate::ops::get_action),
        )
        .route("/v1/admin/ops/outbox", get(crate::ops::list_outbox))
        .route("/v1/admin/ops/deliveries", get(crate::ops::list_deliveries))
        .route(
            "/v1/admin/ops/deliveries/dead/clear",
            post(crate::ops::clear_dead_deliveries),
        )
        .route(
            "/v1/admin/ops/deliveries/{delivery_id}",
            get(crate::ops::delivery_details),
        )
        .route(
            "/v1/admin/ops/outbox/{event_id}/retry",
            post(crate::ops::retry_outbox),
        )
        .route(
            "/v1/admin/ops/deliveries/{delivery_id}/retry",
            post(crate::ops::retry_delivery),
        )
}
