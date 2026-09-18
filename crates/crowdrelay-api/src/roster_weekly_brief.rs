//! The roster's weekly brief — the manager's page (N.7).
//!
//! `GET /v1/admin/roster-plan/weekly-brief?organization_id=…` answers one
//! question for the whole roster at once: who needs a decision, who is
//! drifting, and how fresh each act's own briefing is. The per-act machinery
//! it composes — the approval queue, the lapsed-ask record, the daily
//! briefing, the brain's self-assessment — is the same machinery each act's
//! own surfaces already run on; this read gathers it across the
//! organisation, nothing more.
//!
//! Admin rather than control-plane, for the same reason the roster plan and
//! the pooled channel read are: the control-plane surface is scoped to one
//! tenant by construction, and a roster read accepting a workspace token
//! would be one act reading its labelmates' queues.
//!
//! The read is bounded by the same timeout-and-hold pair the roster plan
//! uses. It is narrower than the plan — a handful of counts over the
//! organisation's queues rather than a funnel per act per city — but it
//! still walks every act's north-star series, and a roster that grows makes
//! it wider without anybody changing it.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::roster_weekly_brief;
use crowdrelay_infra::roster_weekly_brief::act_briefs;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Deserialize)]
pub struct WeeklyBriefParams {
    organization_id: Uuid,
}

/// `GET /v1/admin/roster-plan/weekly-brief`
pub async fn roster_weekly_brief(
    State(state): State<crate::AppState>,
    Query(params): Query<WeeklyBriefParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let request_id_value = request_id(&headers);

    let acts = match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            act_briefs(&state.database, params.organization_id, now),
        ),
    )
    .await
    {
        Ok(Ok(acts)) => acts,
        Ok(Err(error)) => {
            tracing::warn!(%error, "roster weekly brief read failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
        Err(_) => {
            tracing::warn!("roster weekly brief read timed out");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    // An organisation with no members, or members with nothing recorded, is
    // a real page — `compose` returns the empty/zeroed acts, never an error.
    let brief = roster_weekly_brief::compose(params.organization_id, now, acts);

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(brief),
    )
        .into_response()
}
