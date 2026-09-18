//! The band-facing booking-agent surface (§4h-10, §12-5).
//!
//! Mounted under `/v1/control-plane` beside the representation routes. An
//! agent is the booking graph's third entity — a venue sells the band a
//! room, a promoter a night, an agent sells the band — so the approach is
//! an application for a season, not a pitch for a date. The proof is the
//! pitch: the request refuses without real draw numbers behind it, and the
//! agent's address never leaves the platform.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{
    AutopilotBookingAgentStateRepository, RecordBookingAgentReply,
};
use crowdrelay_domain::BookingAgentId;
use crowdrelay_domain::booking_agent::BookingAgentReplyDisposition;
use crowdrelay_infra::booking_agents::{
    BookingAgentApproachOutcome, BookingAgentError, PostgresBookingAgentRepository,
};
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, Problem, request_id};

fn repo(state: &crate::AppState) -> PostgresBookingAgentRepository {
    PostgresBookingAgentRepository::new(state.database.clone())
}

fn booking_agent_problem(error: BookingAgentError, request_id: Option<String>) -> Response {
    match error {
        BookingAgentError::NotFound => Problem::not_found(request_id).private().into_response(),
        BookingAgentError::Refused(detail) => Problem::conflict_owned(detail.into(), request_id)
            .private()
            .into_response(),
        BookingAgentError::Database(error) => {
            tracing::warn!(%error, "booking-agent query failed");
            Problem::service_unavailable(request_id)
                .private()
                .into_response()
        }
    }
}

/// GET — the registry as the band sees it: who the agents are and where the
/// season door stands. No `contact_email` is ever serialized — the platform
/// brokers the send, and an address the band cannot see cannot leak into its
/// own tooling.
pub async fn list_booking_agents(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id().into_uuid();
    match repo(&state).list_agents(workspace_id).await {
        Ok(agents) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(json!({ "agents": agents })),
        )
            .into_response(),
        Err(error) => booking_agent_problem(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingAgentApproachBody {
    agent_id: Uuid,
    #[serde(default)]
    note: Option<String>,
}

/// POST — the band asks to approach an agent. Runs the domain gate and
/// queues an `awaiting_approval` action; dispatch re-runs the same gate
/// under the row lock, so an approval that goes stale cannot send.
pub async fn request_booking_agent_approach(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<BookingAgentApproachBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(idempotency_key) = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
    else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let workspace_id = state.ops.workspace_id().into_uuid();
    match repo(&state)
        .request_approach(
            workspace_id,
            body.agent_id,
            body.note.as_deref(),
            &idempotency_key,
        )
        .await
    {
        Ok(BookingAgentApproachOutcome::Queued { action_id }) => (
            StatusCode::ACCEPTED,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(json!({ "action_id": action_id, "status": "awaiting_approval" })),
        )
            .into_response(),
        Ok(BookingAgentApproachOutcome::Replayed { action_id, status }) => (
            StatusCode::ACCEPTED,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(json!({ "action_id": action_id, "status": status })),
        )
            .into_response(),
        Err(error) => booking_agent_problem(error, request_id_value),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingAgentReplyBody {
    disposition: String,
    /// Required, not defaulted: the operator-action ledger is idempotent on
    /// the request's own content, and a server-invented `now` would make a
    /// retried submit read as a different operation — a 409 where a replay
    /// was owed.
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

/// POST — files what the agent answered. `declined` closes the season's
/// door (`refused_until`), `do_not_contact` is the wall; both ride the same
/// idempotent operator-action machinery as every other recorded reply.
pub async fn record_booking_agent_reply(
    State(state): State<crate::AppState>,
    Path(agent_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<BookingAgentReplyBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(disposition) = BookingAgentReplyDisposition::parse(body.disposition.trim()) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(idempotency_key) = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
    else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let command = RecordBookingAgentReply {
        agent_id: BookingAgentId::from_uuid(agent_id),
        disposition,
        occurred_at: body.occurred_at,
    };
    match state
        .autopilot
        .record_booking_agent_reply(state.ops.workspace_id(), command, &idempotency_key, None)
        .await
    {
        Ok(result) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "private, no-store")],
            Json(result),
        )
            .into_response(),
        Err(error) => match error {
            crowdrelay_application::RepositoryError::NotFound => {
                Problem::not_found(request_id_value)
                    .private()
                    .into_response()
            }
            crowdrelay_application::RepositoryError::Conflict => {
                Problem::conflict(request_id_value)
                    .private()
                    .into_response()
            }
            crowdrelay_application::RepositoryError::ConflictBecause(detail) => {
                Problem::conflict_because(detail, request_id_value)
                    .private()
                    .into_response()
            }
            _ => Problem::service_unavailable(request_id_value)
                .private()
                .into_response(),
        },
    }
}
