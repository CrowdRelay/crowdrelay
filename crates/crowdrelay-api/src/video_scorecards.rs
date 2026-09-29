//! The per-video scorecard routes — the "+1000 CrowdRelay-driven views in 14
//! days" plan read against each recent video.
//!
//! Read-only handlers: the labels and arithmetic live in
//! `crowdrelay_domain::video_scorecard`, the gathering in
//! `crowdrelay_infra::content_scorecard`. Nothing here writes.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::CACHE_CONTROL};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

/// The card feed's page size: ten recent videos by default, thirty at most —
/// the read is a dozen set queries per request, so the page stays small.
const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 30;

#[derive(Deserialize)]
pub struct ScorecardsQuery {
    limit: Option<i64>,
}

/// `GET /v1/control-plane/content/videos/scorecards`
pub async fn list_video_scorecards(
    State(state): State<AppState>,
    Query(query): Query<ScorecardsQuery>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    match tokio::time::timeout(
        state.ops.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            crowdrelay_infra::content_scorecard::list_video_scorecards(
                &state.database,
                state.ops.workspace_id(),
                limit,
            ),
        ),
    )
    .await
    {
        Ok(Ok(cards)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(cards),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "video scorecards read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
        Err(_) => {
            tracing::warn!("video scorecards read timed out");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

/// `GET /v1/control-plane/content/videos/{source_id}/scorecard`
pub async fn video_scorecard(
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
            crowdrelay_infra::content_scorecard::video_scorecard(
                &state.database,
                state.ops.workspace_id(),
                source_id,
            ),
        ),
    )
    .await
    {
        Ok(Ok(Some(card))) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(card),
        )
            .into_response(),
        Ok(Ok(None)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "video scorecard read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
        Err(_) => {
            tracing::warn!("video scorecard read timed out");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
