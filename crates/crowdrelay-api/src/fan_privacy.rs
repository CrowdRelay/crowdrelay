//! Authenticated fan account-erasure endpoint.
//!
//! SQL lives in `crowdrelay-infra`; HTTP owns only authentication/protocol
//! mapping so the API SQL-write ratchet does not regress.

use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use crowdrelay_infra::fan_privacy::{FanPrivacyError, PostgresFanPrivacyRepository};
use serde::Serialize;

use crate::{Problem, acquisition::fan_session_from_headers, request_id};

#[derive(Serialize)]
struct DeleteAccountResponse {
    deleted: bool,
}

#[derive(Serialize)]
struct LeaderboardUnpublishResponse {
    published: bool,
    changed: bool,
}

pub async fn delete_account(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let Some(session) = fan_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let repository = PostgresFanPrivacyRepository::new(state.database.clone());
    match repository
        .erase_account(
            state.ticketing.workspace_id().into_uuid(),
            session.as_str(),
            request_id_value.as_deref(),
        )
        .await
    {
        Ok(_) => Json(DeleteAccountResponse { deleted: true }).into_response(),
        Err(FanPrivacyError::Unauthorized) => Problem::unauthorized(request_id_value)
            .private()
            .into_response(),
        Err(FanPrivacyError::Unexpected) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn unpublish_synesthesia_leaderboard(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(session) = fan_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let repository = PostgresFanPrivacyRepository::new(state.database.clone());
    match repository
        .unpublish_synesthesia_leaderboard(
            state.ticketing.workspace_id().into_uuid(),
            session.as_str(),
            request_id_value.as_deref(),
        )
        .await
    {
        Ok(receipt) => Json(LeaderboardUnpublishResponse {
            published: false,
            changed: receipt.changed,
        })
        .into_response(),
        Err(FanPrivacyError::Unauthorized) => Problem::unauthorized(request_id_value)
            .private()
            .into_response(),
        Err(FanPrivacyError::Unexpected) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}
