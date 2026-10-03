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
    routing::{get, post},
};
use crowdrelay_domain::lane_ledger::verdict;
use serde::Deserialize;
use serde_json::json;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

pub(super) fn control_plane_routes() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/v1/control-plane/growth/lanes", get(list))
        .route("/v1/control-plane/growth/readiness", get(readiness))
        .route(
            "/v1/control-plane/growth/facebook-authority",
            post(set_facebook_authority),
        )
}

/// Can this tenant acquire fans on its own, and if not, what is the one
/// smallest thing in the way? The Day-0 contract's "name the smallest missing
/// prerequisite", in one read. Read-only; grants and changes nothing.
async fn readiness(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match crowdrelay_infra::lane_ledger::day_zero_facts(&state.database, workspace_id).await {
        Ok(facts) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(facts.assess()),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "day-zero readiness read failed");
            Problem::service_unavailable(request_id(&headers)).into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FacebookAuthorityRequest {
    enabled: bool,
}

/// Explicit owner handoff for exactly one owned public rail.
///
/// This endpoint never changes the deployment kill switch and never grants a
/// community/Reddit/Discord capability. Enabling is refused unless the running
/// worker reports its social publisher on and Facebook's connection is
/// currently connected + working with the other Day-0 prerequisites present.
async fn set_facebook_authority(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<FacebookAuthorityRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let rid = request_id(&headers);
    let Ok(Json(request)) = payload else {
        return Problem::bad_request(rid).private().into_response();
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();

    if request.enabled {
        let facts = match crowdrelay_infra::lane_ledger::day_zero_facts(
            &state.database,
            workspace_id,
        )
        .await
        {
            Ok(facts) => facts,
            Err(error) => {
                tracing::warn!(%error, "facebook authority precondition read failed");
                return Problem::service_unavailable(rid).private().into_response();
            }
        };
        if !facts.facebook_authority_grantable() {
            return Problem::conflict_because(
                "Facebook autopost authority was not granted: the signup/copy/fresh-content prerequisites, a connected working Facebook rail, and the worker-reported CROWDRELAY_SOCIAL_AUTO_POST deployment gate must all be ready first.",
                rid,
            )
            .private()
            .into_response();
        }
    }

    let settings =
        crowdrelay_infra::tenant_settings::TenantSettingsRepository::new(state.database.clone());
    if let Err(error) = settings
        .set_facebook_autopost_authority(workspace_id, request.enabled)
        .await
    {
        tracing::warn!(%error, "facebook authority update failed");
        return Problem::service_unavailable(rid).private().into_response();
    }

    // The explicit grant is the last human step. Wake the *existing* Autopilot
    // loop immediately instead of inventing a second execution path or waiting
    // for the next scheduled tick. If NOTIFY is unavailable the durable grant
    // is still valid and the scheduler remains the fallback.
    let cycle_wake = if request.enabled {
        match crowdrelay_infra::autopilot::request_autopilot_cycle(
            &state.database,
            crowdrelay_domain::WorkspaceId::from_uuid(workspace_id),
        )
        .await
        {
            Ok(()) => "requested",
            Err(error) => {
                tracing::warn!(%error, "facebook authority granted but immediate cycle wake failed");
                "scheduled_fallback"
            }
        }
    } else {
        "not_requested"
    };

    match crowdrelay_infra::lane_ledger::day_zero_facts(&state.database, workspace_id).await {
        Ok(facts) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({
                "facebook_authority": if request.enabled { "granted" } else { "revoked" },
                "cycle_wake": cycle_wake,
                "readiness": facts.assess(),
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "facebook authority post-write readiness read failed");
            Problem::service_unavailable(rid).private().into_response()
        }
    }
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
    let days = query
        .days
        .unwrap_or(crowdrelay_domain::lane_ledger::DEFAULT_WINDOW_DAYS);
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
                "scope": lane.scope,
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
