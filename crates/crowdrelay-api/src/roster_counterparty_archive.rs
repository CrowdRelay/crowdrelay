//! The roster archive (5.14, §4h-7.3): every counterparty the
//! organisation's acts have named, in one account.
//!
//! `GET /v1/admin/roster-plan/counterparty-archive?organization_id=…` is
//! the management inbox's promoter half: each counterparty the roster's
//! shows recorded, with the recurrence — how many member acts have named
//! them — because the same promoter recurring across acts is the density
//! the shared registries were built for. A label reading it sees its own
//! history back further than any single act's, while marks from outside
//! the organisation never enter the response.
//!
//! Admin rather than control-plane: an act's counterparties are its own
//! private marks; only the organisation reads them together.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::roster_counterparty_archive::counterparty_archive as compose_archive;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Deserialize)]
pub struct ArchiveParams {
    organization_id: Uuid,
}

/// `GET /v1/admin/roster-plan/counterparty-archive`
pub async fn counterparty_archive(
    State(state): State<crate::AppState>,
    Query(params): Query<ArchiveParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let request_id_value = request_id(&headers);

    match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            compose_archive(&state.database, params.organization_id, now),
        ),
    )
    .await
    {
        Ok(Ok(archive)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(archive),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "counterparty archive read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
        Err(_) => {
            tracing::warn!("counterparty archive read timed out");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
