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
        // P.5: the same one-yes shape as a wave, keyed on the synced post —
        // approve once and the post's whole spread (push + every admitted
        // community) queues together; revoke stops the part not yet running.
        .route(
            "/v1/admin/autopilot/content-sources/{source_id}/relay-ladder/approve",
            post(autopilot::approve_relay_ladder),
        )
        .route(
            "/v1/admin/autopilot/content-sources/{source_id}/relay-ladder/revoke",
            post(autopilot::revoke_relay_ladder),
        )
        // P.4: the show ladder is the same one-yes shape as a wave, keyed on
        // the event rather than a batch — approve once and every rung whose
        // own evidence gates pass fires on schedule; revoke stops the rest.
        .route(
            "/v1/admin/autopilot/events/{event_id}/growth-ladder/approve",
            post(autopilot::approve_show_ladder),
        )
        .route(
            "/v1/admin/autopilot/events/{event_id}/growth-ladder/revoke",
            post(autopilot::revoke_show_ladder),
        )
        .route(
            "/v1/admin/autopilot/playlist-placements",
            post(autopilot::record_playlist_placement),
        )
}
