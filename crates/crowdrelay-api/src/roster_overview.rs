//! The roster view (5.5) — every act's attention, pipeline and gaps on one
//! screen.
//!
//! `GET /v1/admin/roster-plan/overview?organization_id=…` is the standing
//! page the weekly brief is the digest of: per member act, the share of the
//! shared attention budget it spent, the governor state (cooling down, doors
//! closed), what is queued for a human, what shows are coming, and the named
//! gaps — `no_upcoming_show`, `no_briefing`, `no_reachable_fans`. Everything
//! is measured; nothing is inferred from text.
//!
//! Admin rather than control-plane, for the same reason every roster-plan
//! read is: the control-plane surface is scoped to one tenant by
//! construction, and a roster read accepting a workspace token would be one
//! act reading its labelmates' state.
//!
//! The read is bounded by the same timeout-and-hold pair the sibling reads
//! use — seven small queries over the member set, capped at sixty acts.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::roster_overview::roster_overview as compose_overview;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Deserialize)]
pub struct OverviewParams {
    organization_id: Uuid,
}

/// `GET /v1/admin/roster-plan/overview`
pub async fn roster_overview(
    State(state): State<crate::AppState>,
    Query(params): Query<OverviewParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let request_id_value = request_id(&headers);

    let overview = match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            compose_overview(&state.database, params.organization_id, now),
        ),
    )
    .await
    {
        Ok(Ok(overview)) => overview,
        Ok(Err(error)) => {
            tracing::warn!(%error, "roster overview read failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
        Err(_) => {
            tracing::warn!("roster overview read timed out");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(overview),
    )
        .into_response()
}
