//! The label's crossbill (5.15, §4h-7.4): the roster's catalogue rotation.
//!
//! `GET /v1/admin/roster-plan/catalogue-rotation?organization_id=…` is the
//! read: every active `catalogue_rotation` consent edge with headroom, the
//! back-catalogue release it would carry next, and the edges whose
//! catalogues are finished. `POST` runs one — recomputed against the live
//! rows, dispatched through the same capped campaign path every other
//! amplification uses, labelled `catalogue` in the ledger so the spend and
//! the kind are both auditable.
//!
//! Admin rather than control-plane for the usual reason: the rotation
//! spends one act's audience on a labelmate's catalogue, which is the
//! organisation's call, not either act's. Replay is honest rather than
//! keyed: the deliveries ledger is unique on (consent, fan, reference),
//! so a repeated approval of the same edge carries nobody twice — it
//! queues zero and reports zero.

use axum::{
    Json,
    extract::{Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::portfolio::PortfolioError;
use crowdrelay_infra::roster_catalogue_rotation::{
    catalogue_rotation_plan as compose_plan, run_catalogue_rotation as run_rotation,
};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Deserialize)]
pub struct RotationPlanParams {
    organization_id: Uuid,
}

/// `GET /v1/admin/roster-plan/catalogue-rotation`
pub async fn catalogue_rotation_plan(
    State(state): State<crate::AppState>,
    Query(params): Query<RotationPlanParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let request_id_value = request_id(&headers);

    match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            compose_plan(&state.database, params.organization_id, now),
        ),
    )
    .await
    {
        Ok(Ok(plan)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(plan),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "catalogue rotation plan read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
        Err(_) => {
            tracing::warn!("catalogue rotation plan read timed out");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRotationRequest {
    organization_id: Uuid,
    consent_id: Uuid,
}

/// `POST /v1/admin/roster-plan/catalogue-rotation`
pub async fn run_catalogue_rotation(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RunRotationRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let now = OffsetDateTime::now_utc();
    match run_rotation(
        &state.database,
        request.organization_id,
        request.consent_id,
        now,
    )
    .await
    {
        Ok(run) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({
                "consentId": run.consent_id,
                "releaseId": run.release_id,
                "campaignReference": run.campaign_reference,
                "queued": run.queued,
            })),
        )
            .into_response(),
        Err(error) => error_response(error, request_id_value),
    }
}

fn error_response(error: PortfolioError, request_id_value: Option<String>) -> Response {
    match error {
        PortfolioError::NotFound => Problem::not_found(request_id_value).into_response(),
        PortfolioError::CatalogueExhausted => Problem::conflict_because(
            "The edge's catalogue is fully rotated — every released item has already been carried.",
            request_id_value,
        )
        .into_response(),
        PortfolioError::CapReached
        | PortfolioError::InvalidDecision
        | PortfolioError::NotInSameOrganization
        | PortfolioError::Unreciprocated => Problem::conflict(request_id_value).into_response(),
        PortfolioError::Database(_) => {
            Problem::service_unavailable(request_id_value).into_response()
        }
    }
}
