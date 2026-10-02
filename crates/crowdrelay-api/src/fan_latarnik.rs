//! A fan's own view of, and answer to, the Latarnik invitation.
//!
//! The invitation is delivered where the fan already is: their signed-in Signal
//! session. There is no email and no push here — the band asked through an
//! operator decision (`POST /v1/control-plane/growth/latarnik-roles/{id}/status`)
//! and the fan sees the ask when they next open Signal. That is why this lane
//! needs no new outbound channel and no new consent: the fan is consented, the
//! surface is theirs, and nothing is sent to anyone.
//!
//! The session cookie is the only identity accepted; a caller cannot name
//! another fan. The response carries a state and nothing about *why* the band
//! asked — the evidence stays with the band.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::latarnik_roles::{LatarnikError, MyAnswer, answer_my_role, my_role};
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;

use crate::{Problem, acquisition::fan_session_from_headers, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

/// `GET /v1/me/latarnik`
pub async fn get_my_latarnik(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let Some(session) = fan_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    match my_role(
        &state.database,
        state.ticketing.workspace_id().into_uuid(),
        session.as_str(),
    )
    .await
    {
        Ok(Some((role_state, _))) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({ "state": role_state })),
        )
            .into_response(),
        Ok(None) => Problem::unauthorized(request_id_value)
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "latarnik role read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerRequest {
    answer: AnswerWire,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AnswerWire {
    Accept,
    Decline,
    Pause,
    Resume,
    Leave,
}

/// `POST /v1/me/latarnik/answer`
pub async fn answer_my_latarnik(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(body): Json<AnswerRequest>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(session) = fan_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let answer = match body.answer {
        AnswerWire::Accept => MyAnswer::Accept,
        AnswerWire::Decline => MyAnswer::Decline,
        AnswerWire::Pause => MyAnswer::Pause,
        AnswerWire::Resume => MyAnswer::Resume,
        AnswerWire::Leave => MyAnswer::Leave,
    };
    match answer_my_role(
        &state.database,
        state.ticketing.workspace_id().into_uuid(),
        session.as_str(),
        answer,
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(status) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({ "status": status.as_str() })),
        )
            .into_response(),
        Err(LatarnikError::NotFound) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(LatarnikError::IllegalMove { .. }) => Problem::conflict_because(
            "That answer does not fit where the role is now.",
            request_id_value,
        )
        .private()
        .into_response(),
        Err(error) => {
            tracing::warn!(%error, "latarnik answer failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
