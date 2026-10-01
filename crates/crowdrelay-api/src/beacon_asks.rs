//! The beacon human-send lane — control-plane routes for the asks the
//! executor bridge cannot carry (`beacon.outreach` / `beacon.invite_batch`
//! sit in PENDING_ROUTE: no n8n route exists).
//!
//! `GET` lists the asks waiting on a person. `prepare` materializes the
//! letter — verified partner row, campaign touch, live tracked links — and
//! claims the action for `operator-console`. The operator sends it from
//! their own mail client, then `sent` files the terminal receipt through
//! the ordinary execution-report path: the action lands `succeeded` with
//! the same evidence an executor-completed send carries. Sending itself is
//! never the system's to do unattended.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header::CACHE_CONTROL};
use axum::response::{IntoResponse, Json, Response};
use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::{
    AutopilotRuntimeRepository, ExecutorReportStatus, RecordExecutionReport,
};
use crowdrelay_domain::AutopilotActionId;
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

/// `GET /v1/control-plane/beacon-asks`
pub async fn beacon_ask_queue(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    match tokio::time::timeout(
        state.ops.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            crowdrelay_infra::autopilot::list_beacon_ask_queue(
                &state.database,
                state.ops.workspace_id(),
            ),
        ),
    )
    .await
    {
        Ok(Ok(items)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "beacon ask queue read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
        Err(_) => {
            tracing::warn!("beacon ask queue read timed out");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

/// `POST /v1/control-plane/beacon-asks/{action_id}/prepare`
///
/// Materializes the letter: verifies the partner under the executor's own
/// guards, mints the tracked links, claims the action for the operator. The
/// response body is what the operator pastes into their client — the links
/// in it are already live.
pub async fn beacon_ask_prepare(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(action_id) = Uuid::parse_str(action_id.trim()) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match crowdrelay_infra::autopilot::prepare_beacon_ask(
        &state.database,
        state.ops.workspace_id(),
        action_id,
    )
    .await
    {
        Ok(prepared) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(prepared),
        )
            .into_response(),
        Err(RepositoryError::NotFound) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
            Problem::conflict_because(
                "The ask is not waiting on the operator — it may already be claimed, \
                 sent, declined, or still in the approval lane.",
                request_id_value,
            )
            .private()
            .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "beacon ask prepare failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeaconAskSentBody {
    note: Option<String>,
}

/// `POST /v1/control-plane/beacon-asks/{action_id}/sent`
///
/// The operator asserts the letter left their client. The receipt files
/// through the canonical execution-report path under `operator-console`, so
/// the claim, the report row and the `succeeded` transition are the same
/// evidence a provider-completed send carries — with `provider_reference`
/// left `null` on purpose: a person sending from their own client is the
/// system's belief, not the outside world's confirmation.
pub async fn beacon_ask_sent(
    State(state): State<AppState>,
    Path(action_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(action_id) = Uuid::parse_str(action_id.trim()) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let note = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<BeaconAskSentBody>(&body) {
            Ok(body) => body.note.map(|note| note.trim().to_owned()),
            Err(_) => {
                return Problem::bad_request_because(
                    "The body must be empty or a JSON object with an optional `note`.",
                    request_id_value,
                )
                .private()
                .into_response();
            }
        }
    };
    let claim = match crowdrelay_infra::autopilot::beacon_ask_send_claim(
        &state.database,
        state.ops.workspace_id(),
        action_id,
    )
    .await
    {
        Ok(claim) => claim,
        Err(RepositoryError::NotFound) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
            return Problem::conflict_because(
                "The ask was not prepared for the operator, or its claim already closed.",
                request_id_value,
            )
            .private()
            .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "beacon ask send claim failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    match state
        .autopilot
        .record_execution_report(
            state.ops.workspace_id(),
            RecordExecutionReport {
                action_id: AutopilotActionId::from_uuid(action_id),
                receipt_key: format!("operator-sent:{action_id}"),
                executor_id: crowdrelay_infra::autopilot::OPERATOR_EXECUTOR_ID.to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: Some(claim.claim_token),
                provider_reference: None,
                error_kind: None,
                metadata: serde_json::json!({
                    "transport": "operator",
                    "note": note,
                }),
                occurred_at: time::OffsetDateTime::now_utc(),
            },
        )
        .await
    {
        Ok(mutation) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({
                "action_id": mutation.action_id.into_uuid(),
                "status": mutation.status.as_str(),
                "replayed": mutation.replayed,
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "beacon ask send receipt failed");
            repository_problem(error, request_id_value).into_response()
        }
    }
}

fn repository_problem(error: RepositoryError, request_id: Option<String>) -> Problem {
    match error {
        RepositoryError::Unavailable => Problem::service_unavailable(request_id),
        RepositoryError::NotFound => Problem::not_found(request_id),
        RepositoryError::Conflict => Problem::conflict(request_id),
        RepositoryError::ConflictBecause(detail) => Problem::conflict_because(detail, request_id),
        RepositoryError::Unexpected => Problem::internal(request_id),
    }
    .private()
}
