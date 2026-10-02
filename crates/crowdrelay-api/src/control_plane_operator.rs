//! The operator capabilities that lived only under `/v1/admin`.
//!
//! Every route here reuses the canonical admin handler, exactly as
//! `control_plane.rs` does. They were split out because each of them had the
//! same defect: the capability existed, worked, and could be reached only with
//! the admin credential, which the Control Plane deliberately never holds. So
//! a person running the band from the console could not see the ticket sale,
//! the merch stock, the release plan or the fan funnel, and could not change
//! any of them — n8n and `crowdrelayctl` could.
//!
//! What stayed admin-only did so on purpose, and says why in
//! `scripts/test_operator_reachability_v1.py` (`EXPECTED_ADMIN_ONLY`): worker
//! and feed ingestion, financial records, credential minting, permanent
//! deletion, and every organisation-wide roster read — one tenant's token
//! must not read its labelmates' audiences (see `gig_planning::roster_gig_plan`).
//! That list is the decision record; a new `/v1/admin` route that is in
//! neither place fails the gate.
//!
//! The router carries the same route-local ControlPlane guard as
//! `control_plane::router`, and the admin surface's 16 KiB body ceiling rather
//! than the control-plane 8 KiB — these handlers were sized for the admin
//! surface, and a lower ceiling here would refuse a body the handler accepts.

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware::from_fn_with_state,
    routing::{get, post, put},
};

/// Matches `MAX_PUBLIC_BODY_BYTES`, the ceiling the same handlers answer
/// under on `/v1/admin`.
const MAX_OPERATOR_BODY_BYTES: usize = 16 * 1024;

pub(crate) fn router(state: crate::AppState) -> Router {
    Router::new()
        // ── What the brain did and which connections work ─────────────
        // Two of the operator questions CLAUDE.md answers with an endpoint
        // were answered with an admin one.
        .route("/v1/control-plane/ops/cycles", get(crate::ops::list_cycles))
        .route(
            "/v1/control-plane/ops/connections",
            get(crate::ops::list_connection_health),
        )
        // The organic-acquisition funnel is the console's "did that action
        .route("/v1/control-plane/ops/organic-goal", get(crate::ops::organic_goal).post(crate::ops::declare_organic_goal))
        .route("/v1/control-plane/ops/organic-goal/fans/{fan_id}/exclusion", post(crate::ops::set_organic_exclusion))
        // grow fans" read — the same handler the admin surface serves.
        .route(
            "/v1/control-plane/ops/organic-funnel",
            get(crate::ops::organic_funnel),
        )
        // ── Conversion analytics ─────────────────────────────────────
        // The fan-360 half of the North Star: where fans came from, what
        // they paid, and what a referral or an ad actually converted.
        .route(
            "/v1/control-plane/analytics/funnel",
            get(crate::audience::funnel),
        )
        .route(
            "/v1/control-plane/analytics/revenue",
            get(crate::audience::revenue),
        )
        .route(
            "/v1/control-plane/analytics/referral-conversion",
            get(crate::audience::referral_conversion),
        )
        .route(
            "/v1/control-plane/analytics/ad-conversion",
            get(crate::audience::ad_conversion_overview),
        )
        .route(
            "/v1/control-plane/analytics/ad-conversion/breakdown",
            get(crate::audience::ad_conversion_breakdown),
        )
        // ── Fan communications ───────────────────────────────────────
        .route(
            "/v1/control-plane/communications/campaigns",
            get(crate::audience::list_campaigns).post(crate::audience::create_campaign),
        )
        .route(
            "/v1/control-plane/communications/campaigns/{campaign_id}/schedule",
            post(crate::audience::schedule_campaign),
        )
        .route(
            "/v1/control-plane/communications/campaigns/{campaign_id}/cancel",
            post(crate::audience::cancel_campaign),
        )
        // ── Tracked links ────────────────────────────────────────────
        .route(
            "/v1/control-plane/smart-links",
            get(crate::acquisition::admin_list_smart_links)
                .post(crate::acquisition::admin_create_smart_link),
        )
        // The join kit is the console's own checklist: which placement links
        // exist, and what each one returned. The operator pastes the URLs.
        .route(
            "/v1/control-plane/join-kit",
            get(crate::acquisition::admin_join_kit)
                .post(crate::acquisition::admin_ensure_join_kit),
        )
        // ── Tickets, show costs and the night's setup ────────────────
        // The sale's configuration stays admin-only: `configure_sale`
        // re-checks the admin key inside the handler, so price and capacity
        // are a box-office decision whatever namespace reaches it. The read
        // is what the operator was missing.
        .route(
            "/v1/control-plane/events/{event_slug}/ticketing",
            get(crate::ticketing::admin_overview),
        )
        .route(
            "/v1/control-plane/events/{event_slug}/support-slots",
            put(crate::events::set_event_support_slots),
        )
        .route(
            "/v1/control-plane/events/{event_slug}/festival",
            put(crate::events::set_event_festival),
        )
        .route(
            "/v1/control-plane/events/{event_id}/commerce-summary",
            get(crate::commerce::event_merch_summary),
        )
        .route(
            "/v1/control-plane/events/{event_id}/show-cost/prediction",
            post(crate::autopilot::freeze_show_cost_prediction),
        )
        .route(
            "/v1/control-plane/events/{event_id}/show-cost/settlement",
            post(crate::autopilot::settle_show_cost),
        )
        .route(
            "/v1/control-plane/ecosystem/checklists/{event_slug}",
            get(crate::ecosystem::show_checklist),
        )
        .route(
            "/v1/control-plane/ecosystem/checklists/{event_slug}/{item_key}",
            post(crate::ecosystem::update_checklist),
        )
        // ── Concert QR campaigns ─────────────────────────────────────
        .route(
            "/v1/control-plane/event-qr/overview",
            get(crate::concert_qr::overview),
        )
        // The show kit — three placements plus bill-act links in one call —
        // is the console's "make this show scannable" button.
        .route(
            "/v1/control-plane/event-qr/kits",
            post(crate::concert_qr::create_qr_kit),
        )
        .route(
            "/v1/control-plane/event-qr/campaigns",
            get(crate::concert_qr::list_campaigns).post(crate::concert_qr::create_campaign),
        )
        .route(
            "/v1/control-plane/event-qr/campaigns/{campaign_id}/revoke",
            post(crate::concert_qr::revoke_campaign),
        )
        .route(
            "/v1/control-plane/event-qr/campaigns/{campaign_id}/context",
            post(crate::concert_qr::update_campaign_context),
        )
        // ── Merch ────────────────────────────────────────────────────
        .route(
            "/v1/control-plane/merch/catalog",
            get(crate::commerce::admin_catalog).post(crate::commerce::upsert_catalog),
        )
        .route(
            "/v1/control-plane/merch/inventory/overview",
            get(crate::commerce::inventory_overview),
        )
        .route(
            "/v1/control-plane/merch/inventory/activation",
            get(crate::commerce::inventory_activation),
        )
        .route(
            "/v1/control-plane/merch/inventory/stocktakes",
            post(crate::commerce::inventory_stocktake),
        )
        .route(
            "/v1/control-plane/merch/inventory/ready",
            post(crate::commerce::mark_inventory_ready),
        )
        .route(
            "/v1/control-plane/merch/inventory/adjustments",
            post(crate::commerce::adjust_inventory),
        )
        .route(
            "/v1/control-plane/merch/promotion-recommendations",
            get(crate::commerce::promotion_recommendations),
        )
        .route(
            "/v1/control-plane/autopilot/merch-economics",
            post(crate::autopilot::upsert_merch_product_economics),
        )
        .route(
            "/v1/control-plane/autopilot/ticket-allocation-guardrails",
            post(crate::autopilot::upsert_ticket_allocation_guardrail),
        )
        .route(
            "/v1/control-plane/autopilot/promotion-budget-guardrails",
            post(crate::autopilot::upsert_promotion_budget_guardrail),
        )
        // ── Fan rewards ──────────────────────────────────────────────
        // Draws are a conversion mechanism the fan sees; the operator could
        // not schedule one, see who won, or mark a prize as sent. Deleting a
        // draw stays on the admin credential.
        .route(
            "/v1/control-plane/reward-campaigns",
            get(crate::commerce::list_reward_campaigns)
                .post(crate::commerce::create_reward_campaign),
        )
        .route(
            "/v1/control-plane/reward-campaigns/{draw_id}/schedule",
            post(crate::commerce::schedule_reward_campaign),
        )
        .route(
            "/v1/control-plane/reward-campaigns/{draw_id}/cancel",
            post(crate::commerce::cancel_reward_campaign),
        )
        .route(
            "/v1/control-plane/reward-draws",
            get(crate::commerce::list_reward_draws),
        )
        .route(
            "/v1/control-plane/reward-fulfillments",
            get(crate::commerce::list_reward_fulfillments),
        )
        .route(
            "/v1/control-plane/reward-fulfillments/{winner_id}",
            post(crate::commerce::fulfill_reward),
        )
        // ── Releases ─────────────────────────────────────────────────
        .route(
            "/v1/control-plane/autopilot/releases",
            get(crate::autopilot::list_release_plans).post(crate::autopilot::upsert_release_plan),
        )
        .route(
            "/v1/control-plane/autopilot/release-ledger",
            get(crate::autopilot::release_ledger),
        )
        .route(
            "/v1/control-plane/autopilot/release-outcomes",
            get(crate::autopilot::list_release_outcomes),
        )
        .route(
            "/v1/control-plane/autopilot/releases/{release_id}/editorial-pitch",
            post(crate::autopilot::complete_editorial_pitch),
        )
        .route(
            "/v1/control-plane/autopilot/playlist-placements",
            post(crate::autopilot::record_playlist_placement),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-release-campaigns/{campaign_id}/recipients/{beacon_id}",
            post(crate::beacon_signal::admin_update_release_recipient),
        )
        // ── Outreach and booking ─────────────────────────────────────
        // The operator could confirm a discovered candidate but not add a
        // venue they already know, record a promoter's reply, or approve an
        // outreach wave the brain had parked for them.
        .route(
            "/v1/control-plane/autopilot/outreach-waves",
            get(crate::autopilot::list_outreach_waves),
        )
        .route(
            "/v1/control-plane/autopilot/outreach-waves/{wave_id}/approve",
            post(crate::autopilot::approve_outreach_wave),
        )
        .route(
            "/v1/control-plane/autopilot/outreach-targets",
            post(crate::autopilot::upsert_outreach_target),
        )
        .route(
            "/v1/control-plane/autopilot/outreach-targets/{target_id}/reply",
            post(crate::autopilot::record_outreach_reply),
        )
        .route(
            "/v1/control-plane/autopilot/outreach/submission-channels",
            post(crate::autopilot::upsert_submission_channel),
        )
        .route(
            "/v1/control-plane/autopilot/booking-targets",
            post(crate::autopilot::upsert_booking_target),
        )
        .route(
            "/v1/control-plane/autopilot/booking-targets/{target_id}/editions",
            post(crate::autopilot::upsert_festival_edition),
        )
        .route(
            "/v1/control-plane/autopilot/booking-targets/{target_id}/reply",
            post(crate::autopilot::record_booking_reply),
        )
        .route(
            "/v1/control-plane/autopilot/booking-targets/{target_id}/venues/{venue_id}",
            post(crate::autopilot::link_booking_target_venue)
                .delete(crate::autopilot::unlink_booking_target_venue),
        )
        .route(
            "/v1/control-plane/autopilot/manager-config/booking-policy",
            get(crate::autopilot::manager_booking_policy)
                .post(crate::autopilot::set_manager_booking_policy),
        )
        .route(
            "/v1/control-plane/autopilot/team-opportunities/discover",
            post(crate::autopilot::discover_team_opportunity),
        )
        .route(
            "/v1/control-plane/autopilot/actions/{action_id}/assign",
            post(crate::autopilot::assign_action),
        )
        .route(
            "/v1/control-plane/autopilot/content-sources/{source_id}/relay-ladder/approve",
            post(crate::autopilot::approve_relay_ladder),
        )
        .route(
            "/v1/control-plane/autopilot/content-sources/{source_id}/relay-ladder/revoke",
            post(crate::autopilot::revoke_relay_ladder),
        )
        // ── Places ───────────────────────────────────────────────────
        // A registered community could be listed and imported but not opened:
        // its rules, its evidence and its outreach stage were admin-only.
        .route(
            "/v1/control-plane/audience-graph/places/{place_id}",
            get(crate::audience_graph::place_detail),
        )
        .route(
            "/v1/control-plane/audience-graph/places/{place_id}/rules",
            put(crate::audience_graph::attach_rules),
        )
        .route(
            "/v1/control-plane/audience-graph/places/{place_id}/evidence",
            post(crate::audience_graph::append_evidence),
        )
        .route(
            "/v1/control-plane/audience-graph/places/{place_id}/outreach/advance",
            post(crate::audience_graph::advance_outreach),
        )
        // ── Peer acts ────────────────────────────────────────────────
        // Operator tooling rather than the band's console (see
        // `content_engine.rs`): the Control Plane serves these to
        // platform-level sessions only.
        .route(
            "/v1/control-plane/content-engine/peers",
            get(crate::content_engine::list_peers).post(crate::content_engine::create_peer),
        )
        .route(
            "/v1/control-plane/content-engine/peers/{peer_id}/resolve",
            post(crate::content_engine::resolve_peer),
        )
        // ── Portfolio ────────────────────────────────────────────────
        .route(
            "/v1/control-plane/portfolio/amplification/{consent_id}/audience-preview",
            get(crate::portfolio::preview_audience),
        )
        .route(
            "/v1/control-plane/portfolio/amplification/{consent_id}/campaign",
            post(crate::portfolio::run_campaign),
        )
        .route(
            "/v1/control-plane/portfolio/import-fans",
            post(crate::fan_lifecycle::import_fans_admin),
        )
        .route(
            "/v1/control-plane/portfolio/case-study",
            get(crate::portfolio::export_case_study),
        )
        .route_layer(from_fn_with_state(
            state.clone(),
            crate::control_plane::require_control_plane,
        ))
        .layer(DefaultBodyLimit::max(MAX_OPERATOR_BODY_BYTES))
        .with_state(state)
}
