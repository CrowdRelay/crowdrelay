//! Operator surface for roster-level source ROI (5.4, N.6).
//!
//! `GET /v1/admin/roster-plan/source-roi?organization_id=…` answers one
//! question: across every act on this roster, which acquisition channel
//! produces people who are still here thirty days later — and is the gap big
//! enough to move effort on.
//!
//! Admin rather than control-plane, for the same reason the roster plan and the
//! organisation's settings are: the control-plane surface is scoped to one
//! tenant by construction, and this read pools several acts' audiences. A
//! workspace token here would be one act reading its labelmates' funnels.
//!
//! The read is bounded by the same timeout-and-hold pair the roster plan uses.
//! It is a narrower query than the plan — two aggregates over one organisation
//! rather than a funnel per act per city — but it walks every active fan on the
//! roster, and a roster that grows makes it wider without anybody changing it.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::roster_source_roi::{RosterSourceRoi, rank_pooled_channels};
use crowdrelay_infra::roster_source_roi::pooled_channel_counts;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Deserialize)]
pub struct SourceRoiParams {
    organization_id: Uuid,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum SourceRoiResponse {
    /// The organisation has no acts with active fans. Not an error and not an
    /// empty ranking: there is nothing to pool, and saying which is which is
    /// the difference between "set this up" and "this is broken".
    NothingToPool {
        nothing_to_pool: &'static str,
    },
    Ranked(Box<RosterSourceRoi>),
}

/// `GET /v1/admin/roster-plan/source-roi`
pub async fn roster_source_roi(
    State(state): State<crate::AppState>,
    Query(params): Query<SourceRoiParams>,
    headers: HeaderMap,
) -> Response {
    let now = OffsetDateTime::now_utc();
    let request_id_value = request_id(&headers);

    let counts = match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            pooled_channel_counts(&state.database, params.organization_id, now),
        ),
    )
    .await
    {
        Ok(Ok(counts)) => counts,
        Ok(Err(error)) => {
            tracing::warn!(%error, "roster source ROI read failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
        Err(_) => {
            tracing::warn!("roster source ROI read timed out");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let body = if counts.acts_with_fans == 0 {
        SourceRoiResponse::NothingToPool {
            nothing_to_pool: "No act on this roster has an active fan yet, so there is no \
                              channel to rank. This read pools acts; it cannot conjure the \
                              first one.",
        }
    } else {
        SourceRoiResponse::Ranked(Box::new(rank_pooled_channels(&counts)))
    };

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(body),
    )
        .into_response()
}
