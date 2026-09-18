//! Operator surface for the roster's pooled portfolio (5.1).
//!
//! `GET /v1/admin/roster-plan/portfolio?organization_id=…` answers one
//! question: of everything every act on this roster could do this week,
//! which dispatches are worth the slots — ranked once, across all of them,
//! under the organisation's own limits where it has stated any.
//!
//! Admin rather than control-plane, for the same reason the roster plan and
//! source-roi reads are: pooling several acts' candidate pools is exactly
//! what one act's token must not be able to do.
//!
//! The read is bounded by the same timeout-and-hold pair the roster plan
//! uses. It is narrower than the plan — one pool table plus the member list
//! — but it re-ranks a whole roster's candidates, and the response names
//! what could not be ranked rather than dropping it.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::roster_portfolio::{RosterPortfolioPlan, roster_portfolio_plan};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Deserialize)]
pub struct PortfolioParams {
    organization_id: Uuid,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum PortfolioResponse {
    /// The organisation has no member workspaces — there is nothing to pool.
    /// Members with empty pools are a different answer and arrive `Ranked`,
    /// because "three acts, none has run a cycle yet" is information, while
    /// "no acts" is setup that has not happened.
    NothingToPool {
        nothing_to_pool: &'static str,
    },
    Ranked(Box<RosterPortfolioPlan>),
}

/// `GET /v1/admin/roster-plan/portfolio`
pub async fn roster_portfolio(
    State(state): State<crate::AppState>,
    Query(params): Query<PortfolioParams>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);

    let plan = match tokio::time::timeout(
        state.ticketing.operation_timeout(),
        crate::ops::hold(
            &state.read_budget,
            roster_portfolio_plan(&state.database, params.organization_id),
        ),
    )
    .await
    {
        Ok(Ok(plan)) => plan,
        Ok(Err(error)) => {
            tracing::warn!(%error, "roster portfolio read failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
        Err(_) => {
            tracing::warn!("roster portfolio read timed out");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let body = match plan {
        None => PortfolioResponse::NothingToPool {
            nothing_to_pool: "This organisation has no member workspaces, so there is no \
                              pool to rank. The read pools acts; it cannot conjure the \
                              first one.",
        },
        Some(plan) => PortfolioResponse::Ranked(Box::new(plan)),
    };

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(body),
    )
        .into_response()
}
