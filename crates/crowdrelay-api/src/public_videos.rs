//! Public read of an owned video — the data a capture page renders
//! server-side. Read-only: the gathering lives in
//! `crowdrelay_infra::content_scorecard`, nothing here writes.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Json, Response},
};
use serde::Serialize;
use time::OffsetDateTime;

use crate::{AppState, Problem, request_id};

/// A published video's title and publish time change rarely, and a shared
/// capture link can be scraped hard — let edges hold the answer briefly.
const PUBLIC_VIDEO_CACHE: &str = "public, max-age=300";

/// A YouTube video id is eleven id-alphabet characters — anything else is
/// simply not a video this surface knows about.
fn valid_youtube_id(candidate: &str) -> bool {
    candidate.len() == 11
        && candidate
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What the capture page renders: id, title, publish time. Nothing else about
/// the source is a public fact.
#[derive(Serialize)]
struct PublicVideoResponse {
    youtube_id: String,
    title: String,
    #[serde(with = "time::serde::rfc3339")]
    published_at: OffsetDateTime,
}

/// `GET /v1/public/videos/{youtube_id}`
pub async fn public_video(
    State(state): State<AppState>,
    Path(youtube_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let youtube_id = youtube_id.trim();
    if !valid_youtube_id(youtube_id) {
        return Problem::not_found(request_id_value).into_response();
    }
    match tokio::time::timeout(
        state.ops.operation_timeout(),
        crowdrelay_infra::content_scorecard::public_owned_video(
            &state.database,
            state.acquisition.workspace_id(),
            youtube_id,
        ),
    )
    .await
    {
        Ok(Ok(Some(video))) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PUBLIC_VIDEO_CACHE)],
            Json(PublicVideoResponse {
                youtube_id: youtube_id.to_owned(),
                title: video.title,
                published_at: video.published_at,
            }),
        )
            .into_response(),
        Ok(Ok(None)) => Problem::not_found(request_id_value).into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "public video read failed");
            Problem::service_unavailable(request_id_value).into_response()
        }
        Err(_) => {
            tracing::warn!("public video read timed out");
            Problem::service_unavailable(request_id_value).into_response()
        }
    }
}
