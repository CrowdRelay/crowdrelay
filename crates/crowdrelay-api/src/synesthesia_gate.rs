//! Module gate for the synesthesia surface.
//!
//! Synesthesia is Virya's interactive album: an optional tenant module, not a
//! shared product surface. The gate reads the `synesthesia_module` ecosystem
//! flag (default OFF, backfilled ON for pre-existing tenants by migration
//! 0112) and answers disabled requests with a plain 404 so the module's
//! existence is not advertised. Fan privacy actions never pass through here.

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};

pub(crate) const SYNESTHESIA_MODULE_FLAG: &str = "synesthesia_module";

pub(crate) async fn require_synesthesia_module(
    State(state): State<crate::AppState>,
    request: Request,
    next: Next,
) -> Response {
    if crate::ecosystem::module_gate_enabled(&state, SYNESTHESIA_MODULE_FLAG).await {
        return next.run(request).await;
    }
    let request_id_value = crate::request_id(request.headers());
    problem_not_found(request_id_value)
}

fn problem_not_found(request_id: Option<String>) -> axum::response::Response {
    crate::Problem::not_found(request_id).into_response()
}
