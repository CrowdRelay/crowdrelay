//! The Latarnik role's operator surface: who the system thinks is ready to
//! carry the band to a friend, and the one decision a person makes about each.
//!
//! Control-plane-authed. The list carries counts and booleans and a role id —
//! no address and no name, because nothing an operator needs to *decide* depends
//! on them, and this system holds that data on behalf of the fan. The one
//! write moves a role along its status machine; it asks nobody anything (the
//! invitation itself is a separate, gated delivery), it records that a person
//! decided.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crowdrelay_domain::latarnik::RoleStatus;
use crowdrelay_infra::latarnik_roles::{LatarnikError, list_roles, transition_role};
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const LIST_LIMIT: i64 = 200;

pub(super) fn control_plane_routes() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/v1/control-plane/growth/latarnik-roles", get(list))
        .route(
            "/v1/control-plane/growth/latarnik-roles/{role_id}/status",
            post(set_status),
        )
}

async fn list(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match list_roles(&state.database, workspace_id, LIST_LIMIT).await {
        Ok(roles) => {
            let by_status: serde_json::Map<String, serde_json::Value> = RoleStatus::ALL
                .iter()
                .map(|status| {
                    let count = roles.iter().filter(|r| r.status == status.as_str()).count();
                    (status.as_str().to_owned(), json!(count))
                })
                .collect();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(json!({ "by_status": by_status, "roles": roles })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "latarnik role read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetStatus {
    status: String,
    reason: Option<String>,
}

async fn set_status(
    State(state): State<crate::AppState>,
    Path(role_id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<SetStatus>,
) -> Response {
    let Some(next) = RoleStatus::parse(&body.status) else {
        return Problem::bad_request(request_id(&headers)).into_response();
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match transition_role(
        &state.database,
        workspace_id,
        role_id,
        next,
        body.reason.as_deref(),
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(()) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({ "status": next.as_str() })),
        )
            .into_response(),
        Err(LatarnikError::NotFound) => Problem::not_found(request_id(&headers)).into_response(),
        Err(LatarnikError::IllegalMove { .. }) => Problem::conflict_because(
            "The role cannot move to that status from where it is.",
            request_id(&headers),
        )
        .into_response(),
        Err(LatarnikError::ReasonRequired) => {
            Problem::bad_request(request_id(&headers)).into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "latarnik role transition failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}
