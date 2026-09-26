//! Operator surface for the reply lane: the band's drafted answers to the
//! people who commented on its Reddit posts. Protocol mapping only — the
//! statements live in `crowdrelay-infra::fanbase::community_replies`, the
//! rules in `crowdrelay-domain::community_reply`.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::fanbase::{
    CommunityReplyError, approve_community_reply, list_community_replies, skip_community_reply,
};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

fn workspace(state: &crate::AppState) -> Uuid {
    state.ticketing.workspace_id().into_uuid()
}

/// A uniform draw in `[0, 1)` for the send delay. Timing, not security; the
/// midpoint when the OS has nothing to give.
fn unit_draw() -> f64 {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(bytes)) / (f64::from(u32::MAX) + 1.0)
}

fn error_response(error: CommunityReplyError, request_id_value: Option<String>) -> Response {
    match error {
        CommunityReplyError::NotFound => Problem::not_found(request_id_value),
        CommunityReplyError::NotAwaiting(_) => Problem::conflict_because(
            "This reply is not waiting for an answer — it was sent, skipped, or \
             already approved.",
            request_id_value,
        ),
        CommunityReplyError::InvalidDraft => Problem::bad_request(request_id_value),
        CommunityReplyError::Database(error) => {
            tracing::warn!(%error, "community reply write failed");
            Problem::service_unavailable(request_id_value)
        }
    }
    .into_response()
}

/// `GET /v1/control-plane/community-replies` — waiting drafts first.
pub async fn list(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    match list_community_replies(&state.database, workspace(&state)).await {
        Ok(replies) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "replies": replies })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "community replies read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveReplyRequest {
    /// The reply as the operator wants it sent. Omitted: the draft as is.
    #[serde(default)]
    text: Option<String>,
}

/// `POST /v1/control-plane/community-replies/{id}/approve` — send it (as
/// edited). It still leaves only after a human-looking delay and under the
/// account's standing and the reply ceiling.
pub async fn approve(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(reply_id): Path<Uuid>,
    raw: axum::body::Bytes,
) -> Response {
    let request_id_value = request_id(&headers);
    // An empty body approves the draft as it stands; a body must be the
    // strict shape — an unknown field is a mistake, not an approval.
    let body = if raw.iter().all(u8::is_ascii_whitespace) {
        ApproveReplyRequest::default()
    } else {
        match serde_json::from_slice::<ApproveReplyRequest>(&raw) {
            Ok(body) => body,
            Err(_) => return Problem::bad_request(request_id_value).into_response(),
        }
    };
    let not_before = crowdrelay_domain::community_reply::reply_not_before(
        OffsetDateTime::now_utc(),
        unit_draw(),
    );
    match approve_community_reply(
        &state.database,
        workspace(&state),
        reply_id,
        body.text.as_deref(),
        "operator",
        not_before,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

/// `POST /v1/control-plane/community-replies/{id}/skip` — don't answer.
pub async fn skip(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(reply_id): Path<Uuid>,
) -> Response {
    match skip_community_reply(&state.database, workspace(&state), reply_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(error, request_id(&headers)),
    }
}
