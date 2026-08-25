//! The growth-operating-system operator surfaces.
//!
//! Split from `application_routes` because that function is at the size the
//! source ratchet reviews, and because these four belong together: each is a
//! place a human tells the agent something it cannot observe — a promoter's
//! fee, a wave they approve, a curator's claim, and a form only they can
//! submit.

use axum::{
    Router,
    routing::{get, post},
};

use crate::{AppState, autopilot};

pub(super) fn growth_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/admin/autopilot/releases/{release_id}/editorial-pitch",
            post(autopilot::complete_editorial_pitch),
        )
        .route(
            "/v1/admin/autopilot/team-opportunities/{opportunity_id}/terms",
            post(autopilot::record_team_opportunity_terms),
        )
        .route(
            "/v1/admin/autopilot/outreach-waves",
            get(autopilot::list_outreach_waves),
        )
        .route(
            "/v1/admin/autopilot/outreach-waves/{wave_id}/approve",
            post(autopilot::approve_outreach_wave),
        )
        .route(
            "/v1/admin/autopilot/playlist-placements",
            post(autopilot::record_playlist_placement),
        )
}
