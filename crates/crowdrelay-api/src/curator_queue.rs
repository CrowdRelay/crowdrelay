//! The curator DM queue routes — the manual-send lane for channels nobody
//! can post to. `GET` lists the admitted handle candidates with the drafted
//! DM filled for the video; `POST …/sent` records that the operator sent it.
//! Sending itself always happens in the operator's own Telegram client —
//! first contact with a stranger is never the system's to make unattended.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header::CACHE_CONTROL};
use axum::response::{IntoResponse, Json, Response};
use crowdrelay_application::RepositoryError;
use crowdrelay_application::ports::{IdempotencyKey, RequestId};
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

/// `GET /v1/control-plane/content/videos/{source_id}/curator-queue`
pub async fn curator_queue(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(source_id) = Uuid::parse_str(source_id.trim()) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match tokio::time::timeout(
        state.ops.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            crowdrelay_infra::curator_queue::list_curator_queue(
                &state.database,
                state.ops.workspace_id(),
                source_id,
            ),
        ),
    )
    .await
    {
        Ok(Ok(Some(items))) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Ok(None)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "curator queue read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
        Err(_) => {
            tracing::warn!("curator queue read timed out");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct SentBody {
    note: Option<String>,
}

/// `POST /v1/control-plane/content/videos/{source_id}/curator-queue/{candidate_id}/sent`
pub async fn curator_dm_sent(
    State(state): State<AppState>,
    Path((source_id, candidate_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let request_id_value = request_id(&headers);
    let (Ok(source_id), Ok(candidate_id)) = (
        Uuid::parse_str(source_id.trim()),
        Uuid::parse_str(candidate_id.trim()),
    ) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let note = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<SentBody>(&body) {
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
    let Some(idempotency_key) = headers
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
    else {
        return Problem::bad_request_because(
            "The Idempotency-Key header is required: 8 to 128 visible ASCII characters, \
             for example a UUID.",
            request_id_value,
        )
        .private()
        .into_response();
    };
    let request = request_id_value.and_then(|value| RequestId::parse(value).ok());
    match crowdrelay_infra::curator_queue::mark_curator_dm_sent(
        &state.database,
        state.ops.workspace_id(),
        source_id,
        candidate_id,
        note.as_deref(),
        &idempotency_key,
        request.as_ref(),
    )
    .await
    {
        Ok(sent) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(sent),
        )
            .into_response(),
        Err(RepositoryError::NotFound) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(RepositoryError::Conflict) => Problem::conflict_because(
            "The candidate is not an admitted handle — it may have been refused or \
             already handled since the queue was read.",
            request_id(&headers),
        )
        .private()
        .into_response(),
        Err(error) => {
            tracing::warn!(%error, "curator send record failed");
            Problem::service_unavailable(request_id(&headers))
                .private()
                .into_response()
        }
    }
}
