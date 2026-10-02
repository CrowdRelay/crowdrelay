//! The person funnel: how many publicly observed people the band holds, where
//! they are in the relationship, and which source classes they came from.
//!
//! Read-only and control-plane-authed. The funnel reports counts only. The
//! separate action queue is deliberately private/no-store and includes the
//! public identity because an operator cannot review "reply to this person"
//! without knowing which public conversation it refers to. An empty
//! workspace reports an empty funnel with every status present, so "no
//! prospects" and "the read failed" cannot be confused (a failed read is a 503).

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::get,
};
use crowdrelay_domain::fan_prospect::ProspectStatus;
use serde_json::json;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

pub(super) fn control_plane_routes() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/v1/control-plane/growth/prospect-funnel", get(funnel))
        .route("/v1/control-plane/growth/prospect-actions", get(actions))
}

async fn actions(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match crowdrelay_infra::fan_prospects::next_actions(&state.database, workspace_id).await {
        Ok(items) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({ "items": items })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "prospect next-action read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

async fn funnel(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let (statuses, sources) = match tokio::try_join!(
        crowdrelay_infra::fan_prospects::status_counts(&state.database, workspace_id),
        crowdrelay_infra::fan_prospects::source_counts(&state.database, workspace_id),
    ) {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(%error, "prospect funnel read failed");
            return Problem::service_unavailable(request_id(&headers)).into_response();
        }
    };
    let by_status: serde_json::Map<String, serde_json::Value> = ProspectStatus::ALL
        .iter()
        .map(|status| {
            let count = statuses
                .iter()
                .find(|row| row.status == status.as_str())
                .map_or(0, |row| row.prospects);
            (status.as_str().to_owned(), json!(count))
        })
        .collect();
    let total: i64 = statuses.iter().map(|row| row.prospects).sum();
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(json!({
            "prospects": total,
            "by_status": by_status,
            "by_source": sources
                .iter()
                .map(|row| json!({
                    "platform": row.platform,
                    "source": row.source,
                    "prospects": row.prospects,
                    "converted": row.converted,
                }))
                .collect::<Vec<_>>(),
        })),
    )
        .into_response()
}
