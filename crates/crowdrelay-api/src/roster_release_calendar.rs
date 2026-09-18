//! The roster's release calendar (5.16) — the collision rule's second
//! dimension, on the same surface as the rest of the roster plan.
//!
//! `GET /v1/admin/roster-plan/release-calendar?organization_id=…` answers
//! the question a label asks every quarter: which of my acts' releases
//! land on each other, and which one should move. Weeks holding a single
//! release are the answer to "is next month clear"; weeks holding two get
//! a proposal — who keeps the week, who moves, and the reason, because a
//! calendar that reorders silently is an argument, not a decision.
//!
//! Admin rather than control-plane for the usual reason: the calendar is
//! the organisation's releases read together, and a workspace token here
//! would be one act reading its labelmates' unannounced dates.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::roster_release_calendar::roster_release_calendar as compose_calendar;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Deserialize)]
pub struct ReleaseCalendarParams {
    organization_id: Uuid,
}

/// `GET /v1/admin/roster-plan/release-calendar`
pub async fn roster_release_calendar(
    State(state): State<crate::AppState>,
    Query(params): Query<ReleaseCalendarParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let request_id_value = request_id(&headers);

    match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            compose_calendar(&state.database, params.organization_id, now),
        ),
    )
    .await
    {
        Ok(Ok(calendar)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(calendar),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "roster release calendar read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
        Err(_) => {
            tracing::warn!("roster release calendar read timed out");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
