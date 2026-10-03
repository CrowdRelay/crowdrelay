//! The scout lane's halt, and the one way out of it: a person looking.
//!
//! `GET` says whether the lane is halted and why (the same read the reply senders
//! consult, so the answer cannot differ from what the lane does). `POST
//! …/acknowledge` records that a person reviewed one kind of breach, with the
//! reason resuming is safe; only touches made after it can breach that kind
//! again. Control-plane-authed. There is deliberately no "resume" that skips the
//! reason, and no acknowledgement of a kind that is not currently breached is
//! needed or accepted as a pre-clearance.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crowdrelay_infra::scout_lane::{Breach, acknowledge, breaches};
use serde::Deserialize;
use serde_json::json;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

pub(super) fn control_plane_routes() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/v1/control-plane/growth/scout-lane", get(status))
        .route(
            "/v1/control-plane/growth/scout-lane/acknowledge",
            post(acknowledge_breach),
        )
}

async fn status(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    match breaches(&state.database, state.ticketing.workspace_id().into_uuid()).await {
        Ok(found) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({
                "halted": !found.is_empty(),
                "breaches": found.iter().map(|b| json!({
                    "key": b.key(),
                    "acknowledge_as": b.ack_key(),
                })).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "scout lane read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcknowledgeRequest {
    breach: String,
    note: String,
    acknowledged_by: String,
}

async fn acknowledge_breach(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(body): Json<AcknowledgeRequest>,
) -> Response {
    let rid = request_id(&headers);
    let Some(breach) = Breach::ALL
        .into_iter()
        .find(|b| b.ack_key() == body.breach || b.key() == body.breach)
    else {
        return Problem::bad_request(rid).private().into_response();
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let note = body.note.trim();
    let by = body.acknowledged_by.trim();
    if note.is_empty() || note.chars().count() > 1_000 || by.is_empty() || by.chars().count() > 120
    {
        return Problem::bad_request(rid).private().into_response();
    }
    // Only a breach that is actually standing can be acknowledged: this is a
    // review of something seen, not a standing exemption.
    match breaches(&state.database, workspace_id).await {
        Ok(found) if found.contains(&breach) => {}
        Ok(_) => {
            return Problem::conflict_because(
                "That breach is not currently standing, so there is nothing to acknowledge.",
                rid,
            )
            .private()
            .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "scout lane read failed");
            return Problem::service_unavailable(rid).private().into_response();
        }
    }
    match acknowledge(&state.database, workspace_id, breach, by, note).await {
        Ok(()) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({ "acknowledged": breach.ack_key() })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "scout breach acknowledgement failed");
            Problem::service_unavailable(rid).private().into_response()
        }
    }
}
