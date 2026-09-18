//! The label's quarterly page to one of its own acts (5.25).
//!
//! `GET /v1/admin/roster-plan/act-report?organization_id=…&workspace_id=…`
//! composes the quarter's record for one member act — actions dispatched,
//! rooms played, fans gained by city, catalogue rotations landed — from the
//! same tables the act's own surfaces read. The page exists so a label can
//! show an act what it did; the membership check is the boundary, because a
//! page about an act outside the organisation is somebody else's record.
//!
//! Admin rather than control-plane, for the same reason the rest of this
//! surface is: the report reads the organisation's membership, and a
//! workspace token here would be one act deciding which label it reports
//! for.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::roster_act_report::roster_act_report as compose_act_report;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Deserialize)]
pub struct ActReportParams {
    organization_id: Uuid,
    workspace_id: Uuid,
}

/// `GET /v1/admin/roster-plan/act-report`
pub async fn roster_act_report(
    State(state): State<crate::AppState>,
    Query(params): Query<ActReportParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let request_id_value = request_id(&headers);

    match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            compose_act_report(
                &state.database,
                params.organization_id,
                params.workspace_id,
                now,
            ),
        ),
    )
    .await
    {
        // A non-member workspace is not an error — it is the honest "no
        // page": the organisation has no report to give for an act that is
        // not on it. 404 rather than an empty page, because an empty page
        // would claim membership that does not exist.
        Ok(Ok(None)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Ok(Some(report))) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(report),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "roster act report read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
        Err(_) => {
            tracing::warn!("roster act report read timed out");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
