//! Where each delivery lane stops: the P0.4 answer to "what happens to what we
//! ask a lane to do".
//!
//! Read-only, control-plane-authed. Every lane carries its counts, its oldest
//! unfinished request and the one-line verdict; a lane nobody asked anything of
//! is `quiet` (measured zero, not health). The tenant's own autopost switches
//! ride beside the lanes so "held for a person" can be read against "the tenant
//! never turned autopost on for this platform" — a lane working as designed — as
//! opposed to an executor that was supposed to publish and did not.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::get,
};
use crowdrelay_domain::lane_ledger::verdict;
use serde::Deserialize;
use serde_json::json;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const DEFAULT_DAYS: i32 = 14;

pub(super) fn control_plane_routes() -> axum::Router<crate::AppState> {
    axum::Router::new().route("/v1/control-plane/growth/lanes", get(list))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    days: Option<i32>,
}

async fn list(
    State(state): State<crate::AppState>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let days = query.days.unwrap_or(DEFAULT_DAYS);
    if !(1..=90).contains(&days) {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let (lanes, settings) = match tokio::try_join!(
        crowdrelay_infra::lane_ledger::lane_rows(&state.database, workspace_id, days),
        crowdrelay_infra::lane_ledger::autopost_settings(&state.database, workspace_id),
    ) {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(%error, "lane ledger read failed");
            return Problem::service_unavailable(request_id(&headers)).into_response();
        }
    };
    let lanes: Vec<_> = lanes
        .iter()
        .map(|lane| {
            let lane_verdict = verdict(&lane.counts);
            json!({
                "lane": lane.lane,
                "verdict": lane_verdict,
                "needs_attention": lane_verdict.needs_attention(),
                "counts": lane.counts,
                "asked": lane.counts.asked(),
                "oldest_unfinished_hours": lane.oldest_unfinished_hours,
                "unknown_statuses": lane.unknown_statuses,
            })
        })
        .collect();
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(json!({
            "window_days": days,
            // A lane absent from this list was asked nothing in the window.
            "lanes": lanes,
            // As stored; null means no row (the domain default applies), not off.
            "tenant_autopost": {
                "social_auto_post": settings.social_auto_post,
                "social_autopost_platforms": settings.social_autopost_platforms,
            },
        })),
    )
        .into_response()
}
