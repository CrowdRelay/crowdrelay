//! Narrow server-to-server surface used by the multi-tenant Control Plane.
//!
//! This namespace deliberately reuses canonical admin handlers while exposing
//! only operational reads and bounded feature/autonomy mutations. It has its
//! own credential and must never grow into an alias for `/v1/admin`.

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Request, header::CACHE_CONTROL},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Serialize;
use uuid::Uuid;

const MAX_CONTROL_BODY_BYTES: usize = 8 * 1024;
/// Body limit for the event bill replacement alone.
///
/// The handler accepts up to 32 acts with names and per-act ticket URLs, so
/// the router-wide 8 KiB would reject a full valid bill the staff surface
/// (16 KiB) accepts. Sized to match that surface exactly.
const MAX_EVENT_BILL_BODY_BYTES: usize = 16 * 1024;
/// Body limit for the audience-graph bulk import alone.
///
/// `MAX_IMPORT_PLACES` is 500, and a place carries a name, URL, genres and
/// notes, so the router-wide 8 KiB would reject a payload a quarter of the
/// handler's own cap. Sized to let the handler's limit be the real one.
const MAX_IMPORT_BODY_BYTES: usize = 512 * 1024;
/// The sheet upload carries up to 2 MiB of CSV plus the JSON envelope —
/// the router-wide 8 KiB would refuse every real sheet at the door before
/// the handler's own bounds ran. The contract test reviews this ceiling.
const MAX_UPLOAD_BODY_BYTES: usize = 2112 * 1024;

pub(crate) fn router(state: crate::AppState) -> Router {
    Router::new()
        .route("/v1/control-plane/ops/summary", get(crate::ops::summary))
        .route(
            "/v1/control-plane/ops/signal-overview",
            get(crate::ops::signal_overview),
        )
        .route(
            "/v1/control-plane/ops/attention",
            get(crate::ops::attention),
        )
        // The outward funnel: proposals → approvals → sends → replies per
        // channel, with internal housekeeping counted separately so motion
        // cannot pass for growth.
        .route("/v1/control-plane/ops/funnel", get(crate::ops::funnel))
        // What the approved asks produced: terminal actions with their
        // measurement verdicts — the funnel's other half, per action.
        .route("/v1/control-plane/ops/outcomes", get(crate::ops::outcomes))
        // The goal scoreboard: planned vs actual for the objective the brain
        // is working toward, resolved-evidence count against the learning
        // target, and how long people take to approve what it drafts.
        .route("/v1/control-plane/ops/goal", get(crate::ops::goal))
        // The intelligence brief: one read composing the brain's verdict,
        // its posture, its plan, what it found, what it did, and what needs
        // the operator — the "are we getting anywhere" answer.
        .route(
            "/v1/control-plane/ops/intelligence",
            get(crate::ops::intelligence),
        )
        // Where the fans came from: the attribution snapshots the worker
        // writes once per cycle, observed/incremental/durable per template
        // and per strategy.
        .route(
            "/v1/control-plane/ops/fan-sources",
            get(crate::ops::fan_sources),
        )
        .route("/v1/control-plane/ops/outbox", get(crate::ops::list_outbox))
        .route(
            "/v1/control-plane/ops/outbox/{event_id}/retry",
            post(crate::ops::retry_outbox),
        )
        .route(
            "/v1/control-plane/ops/deliveries",
            get(crate::ops::list_deliveries),
        )
        .route(
            "/v1/control-plane/ops/deliveries/dead/clear",
            post(crate::ops::clear_dead_deliveries),
        )
        .route(
            "/v1/control-plane/ops/deliveries/{delivery_id}",
            get(crate::ops::delivery_details),
        )
        .route(
            "/v1/control-plane/ops/deliveries/{delivery_id}/retry",
            post(crate::ops::retry_delivery),
        )
        .route(
            "/v1/control-plane/ops/push/{delivery_id}/retry",
            post(crate::ops::retry_push),
        )
        .route(
            "/v1/control-plane/ops/operations/{request_id}",
            get(crate::ops::operation_timeline),
        )
        .route(
            "/v1/control-plane/ops/trace/{trace_id}",
            get(crate::ops::trace_timeline),
        )
        .route(
            "/v1/control-plane/ops/actions",
            get(crate::ops::list_actions),
        )
        .route(
            "/v1/control-plane/ops/actions/{action_id}",
            get(crate::ops::get_action),
        )
        .route(
            "/v1/control-plane/ops/action-states",
            get(crate::ops::action_states),
        )
        .route(
            "/v1/control-plane/ops/delivery-results",
            get(crate::ops::list_delivery_results),
        )
        // Process-run read models: one pass of a pipeline over one subject,
        // joined into the step shape the process pages render. The community
        // relay is the first kind.
        .route(
            "/v1/control-plane/processes/relays",
            get(crate::ops::process_relays),
        )
        .route(
            "/v1/control-plane/processes/relays/{source_id}",
            get(crate::ops::process_relay_run),
        )
        .route(
            "/v1/control-plane/ecosystem/overview",
            get(crate::ecosystem::overview),
        )
        .route(
            "/v1/control-plane/ecosystem/findings",
            get(crate::ecosystem::list_findings),
        )
        .route(
            "/v1/control-plane/ecosystem/reconcile",
            post(crate::ecosystem::reconcile),
        )
        .route(
            "/v1/control-plane/ecosystem/flags",
            get(crate::ecosystem::list_flags),
        )
        .route(
            "/v1/control-plane/ecosystem/flags/{key}",
            post(crate::ecosystem::update_flag),
        )
        // The operator's third show write: a label that never ran a
        // sync source types the night in by hand. Same handler
        // staff/admin mount. (The comment sits above the route because a
        // line comment inside the verb chain breaks the reachability
        // contract's verb regex.)
        .route(
            "/v1/control-plane/events",
            get(crate::concert_qr::control_plane_events)
                .post(crate::events::create_event)
                .layer(DefaultBodyLimit::max(MAX_EVENT_BILL_BODY_BYTES)),
        )
        .route(
            "/v1/control-plane/events/{event_slug}/timeline",
            get(crate::concert_qr::control_plane_event_timeline),
        )
        .route(
            "/v1/control-plane/events/{event_slug}/scan",
            get(crate::concert_qr::control_plane_event_scan),
        )
        .route(
            "/v1/control-plane/events/{event_slug}/report",
            get(crate::concert_qr::control_plane_event_report),
        )
        // §4h-11: the contacts who could fill this room, read against the
        // date. As an inventory nobody opens the list; against Friday in
        // Wrocław, a Wrocław paper is worth a minute this week.
        .route(
            "/v1/control-plane/events/{event_slug}/who-can-help",
            get(crate::concert_qr::control_plane_event_helpers),
        )
        // The operator's two show writes: the bill (crossbill's input — the
        // timeline already renders it) and the counterparty the T+7 report
        // ships to. Same handlers staff/admin mount; the control-plane
        // prefix is what puts them on the operator's credential.
        .route(
            "/v1/control-plane/events/{event_slug}/acts",
            put(crate::events::replace_event_acts)
                .layer(DefaultBodyLimit::max(MAX_EVENT_BILL_BODY_BYTES)),
        )
        .route(
            "/v1/control-plane/events/{event_slug}/counterparty",
            put(crate::events::set_event_counterparty),
        )
        .route(
            "/v1/control-plane/autopilot/cycle/preview",
            get(crate::autopilot::preview_autopilot_cycle),
        )
        .route(
            "/v1/control-plane/autopilot/cycle/run",
            post(crate::autopilot::run_autopilot_cycle),
        )
        .route(
            "/v1/control-plane/autopilot/overview",
            get(crate::autopilot::overview),
        )
        .route(
            "/v1/control-plane/autopilot/growth",
            get(crate::autopilot::growth),
        )
        .route(
            "/v1/control-plane/autopilot/policies/{context}",
            post(crate::autopilot::set_authority),
        )
        // The opportunity board: what the agent found and parked, then the two
        // decisions a human can make about one finding. "Do it" approves the
        // parked action through the existing approval handler and "done
        // ourselves" records a human took it outside the system — both reuse
        // the canonical admin handlers verbatim, so this surface grows no
        // authority path of its own.
        .route(
            "/v1/control-plane/autopilot/next-best-actions",
            get(crate::autopilot::next_best_actions),
        )
        // The scout shortlist: every tracked opportunity with its link,
        // costed figures, staleness and newest decision — the review surface
        // scout findings land on.
        .route(
            "/v1/control-plane/autopilot/opportunity-shortlist",
            get(crate::autopilot::opportunity_shortlist),
        )
        .route(
            "/v1/control-plane/autopilot/scorecard",
            get(crate::autopilot::scorecard_handler),
        )
        .route(
            "/v1/control-plane/autopilot/measurement",
            get(crate::autopilot::measurement_handler),
        )
        .route(
            "/v1/control-plane/autopilot/reply-triage",
            get(crate::autopilot::reply_triage_handler),
        )
        // The outreach conversation list — every press, radio, venue and
        // agent contact with where the conversation stands — and the act
        // saying it wrote back from its own mailbox, which is what moves a
        // conversation out of "your turn".
        .route(
            "/v1/control-plane/autopilot/outreach-contacts",
            get(crate::autopilot::list_outreach_contacts),
        )
        .route(
            "/v1/control-plane/autopilot/outreach-targets/{target_id}/written",
            post(crate::autopilot::record_outreach_written),
        )
        // The drawer's read: one contact's whole thread — every message either
        // way, the letters the machine drafted or sent, and what happens next
        // per the evaluator's own rules.
        .route(
            "/v1/control-plane/autopilot/outreach-targets/{target_id}/conversation",
            get(crate::autopilot::get_outreach_conversation),
        )
        // "Log their answer" is already registered on this prefix in
        // `control_plane_operator.rs` — the drawer uses that route.
        // "Don't contact" — the contact's standing changes, the ledger does
        // not pretend a reply arrived.
        .route(
            "/v1/control-plane/autopilot/outreach-targets/{target_id}/suppression",
            post(crate::autopilot::suppress_outreach_target),
        )
        // The negotiation table: every live terms conversation with the
        // ladder it was argued from and the move parked for approval. The
        // position write reuses the canonical admin handler — this surface
        // grows no authority path of its own.
        .route(
            "/v1/control-plane/autopilot/negotiations",
            get(crate::autopilot::negotiations),
        )
        // P.4: the show's growth ladder — the approve-once read plus the two
        // writes. Both writes reuse the canonical admin handlers; this surface
        // only routes to them.
        .route(
            "/v1/control-plane/autopilot/events/{event_id}/growth-ladder",
            get(crate::autopilot::show_ladder),
        )
        .route(
            "/v1/control-plane/autopilot/events/{event_id}/growth-ladder/approve",
            post(crate::autopilot::approve_show_ladder),
        )
        .route(
            "/v1/control-plane/autopilot/events/{event_id}/growth-ladder/revoke",
            post(crate::autopilot::revoke_show_ladder),
        )
        // Community relay batches: a content piece's whole community spread as
        // one card — image, targets, the hourly drip — instead of a parked
        // action per community. The writes reuse the canonical handlers; this
        // surface only routes to them.
        .route(
            "/v1/control-plane/autopilot/community-relays",
            get(crate::autopilot::list_community_relays),
        )
        .route(
            "/v1/control-plane/autopilot/community-relays/{source_id}/approve",
            post(crate::autopilot::approve_community_relay),
        )
        .route(
            "/v1/control-plane/autopilot/community-relays/{source_id}/revoke",
            post(crate::autopilot::revoke_community_relay),
        )
        // Per-video scorecards: each new video's march on "+1000
        // CrowdRelay-driven views in 14 days" — attributed views where the
        // Analytics split exists, the click floor where it does not, and the
        // ordered list of what is missing.
        .route(
            "/v1/control-plane/content/videos/scorecards",
            get(crate::video_scorecards::list_video_scorecards),
        )
        .route(
            "/v1/control-plane/content/videos/{source_id}/scorecard",
            get(crate::video_scorecards::video_scorecard),
        )
        // The curator queue: admin handles for channels nobody can post to,
        // listed per video with the drafted DM. `sent` records that the
        // operator sent it — the DM itself always leaves from the
        // operator's own client.
        .route(
            "/v1/control-plane/content/videos/{source_id}/curator-queue",
            get(crate::curator_queue::curator_queue),
        )
        .route(
            "/v1/control-plane/content/videos/{source_id}/curator-queue/{candidate_id}/sent",
            post(crate::curator_queue::curator_dm_sent),
        )
        .route(
            "/v1/control-plane/autopilot/team-opportunities/{opportunity_id}/terms",
            post(crate::autopilot::record_team_opportunity_terms),
        )
        // The shortlist's close-out control — same reuse: the admin handler
        // owns the write, this surface only routes to it.
        .route(
            "/v1/control-plane/autopilot/team-opportunities/{opportunity_id}/progress",
            post(crate::autopilot::record_team_opportunity_progress),
        )
        .route(
            "/v1/control-plane/autopilot/actions/{action_id}/approve",
            post(crate::autopilot::approve_action),
        )
        // Fix a waiting draft's words without approving it — a wave pitch
        // can only be approved with its batch.
        .route(
            "/v1/control-plane/autopilot/actions/{action_id}/revise",
            post(crate::autopilot::revise_action_draft),
        )
        // The same decision, several at a time. Approvals expire at 72 hours
        // and the queue refills every cycle; answering them one at a time is
        // a race the operator loses.
        .route(
            "/v1/control-plane/autopilot/actions/approve",
            post(crate::autopilot::approve_actions),
        )
        .route(
            "/v1/control-plane/autopilot/actions/{action_id}/cancel",
            post(crate::autopilot::cancel_action),
        )
        // What the action actually sent — the words and the addresses. The
        // read that makes the *next* approval easier to give.
        .route(
            "/v1/control-plane/autopilot/actions/{action_id}/sent",
            get(crate::autopilot::action_sent_record),
        )
        .route(
            "/v1/control-plane/autopilot/decisions/{decision_id}/handled-externally",
            post(crate::autopilot::mark_decision_handled_externally),
        )
        // Decision evidence: structured "why this decision" data from the
        // persisted decision row. Read-only — no authority path of its own.
        .route(
            "/v1/control-plane/autopilot/decisions/{decision_id}/evidence",
            get(crate::autopilot::decision_evidence),
        )
        // Learning loop: last 20 decisions with their actions and outcomes.
        // The real decision → action → outcome chain, not fabricated.
        .route(
            "/v1/control-plane/autopilot/learning-loop",
            get(crate::autopilot::learning_loop),
        )
        // Learning proof: the belief revisions themselves — what the brain
        // changed, what changed it, and which later decisions acted on the
        // change. The link `learning-loop` cannot show.
        .route(
            "/v1/control-plane/autopilot/learning-proof",
            get(crate::autopilot::learning_proof),
        )
        // Label portfolio: roster KPIs and the consent-edge decisions. Same
        // handlers as the admin surface, so the control plane grows no
        // authority path of its own.
        .route(
            "/v1/control-plane/portfolio/overview",
            get(crate::portfolio::portfolio_overview),
        )
        .route(
            "/v1/control-plane/portfolio/amplification",
            // Proposing was admin-only, so a label could accept or decline
            // another act's ask from the console but never make one.
            get(crate::portfolio::list_amplification).post(crate::portfolio::propose_amplification),
        )
        .route(
            "/v1/control-plane/portfolio/amplification/{consent_id}/decide",
            post(crate::portfolio::decide_amplification),
        )
        .route(
            "/v1/control-plane/tenant-settings/north-stars",
            get(crate::tenant_settings_http::list_north_star_options),
        )
        .route(
            "/v1/control-plane/tenant-settings/intents",
            get(crate::tenant_settings_http::list_tenant_intent_options),
        )
        .route(
            "/v1/control-plane/tenant-settings",
            get(crate::tenant_settings_http::get_brand_settings),
        )
        .route(
            "/v1/control-plane/tenant-settings/{key}",
            post(crate::tenant_settings_http::upsert_setting),
        )
        // Tenant-held credentials (Stripe keys first): the masked list, the
        // write-only set, and the unset. A stored value can be replaced or
        // removed here but never read back — the reveal is the internal
        // route's, over the commerce credential.
        .route(
            "/v1/control-plane/secrets",
            get(crate::workspace_secrets_http::list_secrets),
        )
        .route(
            "/v1/control-plane/secrets/{name}",
            put(crate::workspace_secrets_http::put_secret)
                .delete(crate::workspace_secrets_http::delete_secret),
        )
        .route(
            "/v1/control-plane/fanbases",
            get(crate::fanbase::list_fanbases).post(crate::fanbase::create_fanbase),
        )
        .route(
            "/v1/control-plane/fanbases/{fanbase_id}",
            axum::routing::delete(crate::fanbase::delete_fanbase),
        )
        .route(
            "/v1/control-plane/fanbases/{fanbase_id}/ingest",
            post(crate::fanbase::ingest_fanbase),
        )
        .route(
            "/v1/control-plane/fanbases/connections",
            get(crate::fanbase::list_fanbase_connections),
        )
        .route(
            "/v1/control-plane/fanbases/connections/{connection_id}",
            axum::routing::delete(crate::fanbase::delete_fanbase_connection),
        )
        .route(
            "/v1/control-plane/fanbases/connections/{connection_id}/scan-scope",
            axum::routing::patch(crate::fanbase::update_connection_scan_scope),
        )
        .route(
            "/v1/control-plane/connections/discord",
            post(crate::connections_simple::create_discord_connection),
        )
        .route(
            "/v1/control-plane/connections/telegram",
            post(crate::connections_simple::create_telegram_connection),
        )
        .route(
            "/v1/control-plane/connections/lastfm",
            post(crate::connections_simple::create_lastfm_connection),
        )
        .route(
            "/v1/control-plane/connections/deezer",
            post(crate::connections_simple::create_deezer_connection),
        )
        .route(
            "/v1/control-plane/connections/discogs",
            post(crate::connections_simple::create_discogs_connection),
        )
        .route(
            "/v1/control-plane/connections/bluesky",
            post(crate::connections_simple::create_bluesky_connection),
        )
        .route(
            "/v1/control-plane/connections/bandcamp",
            post(crate::connections_simple::create_bandcamp_connection),
        )
        .route(
            "/v1/control-plane/connections/youtube",
            post(crate::connections_simple::create_youtube_connection),
        )
        .route(
            "/v1/control-plane/connections/facebook",
            post(crate::connections_simple::create_facebook_connection),
        )
        .route(
            "/v1/control-plane/connections/instagram",
            post(crate::connections_simple::create_instagram_connection),
        )
        .route(
            "/v1/control-plane/connections/soundcloud",
            post(crate::connections_simple::create_soundcloud_connection),
        )
        .route(
            "/v1/control-plane/connections/reddit",
            post(crate::connections_simple::create_reddit_connection),
        )
        .route(
            "/v1/control-plane/gdrive/contacts",
            get(crate::gdrive::list_contacts),
        )
        .route(
            "/v1/control-plane/gdrive/scan",
            post(crate::gdrive::scan_now),
        )
        // P.2: the operator's own sheet is an intake source too — staged
        // through the same extractor the connectors feed.
        .route(
            "/v1/control-plane/gdrive/contacts/upload",
            post(crate::gdrive::upload_contacts)
                .layer(DefaultBodyLimit::max(MAX_UPLOAD_BODY_BYTES)),
        )
        .route(
            "/v1/control-plane/gdrive/contacts/promote-batch",
            post(crate::gdrive::promote_batch),
        )
        .route(
            "/v1/control-plane/gdrive/contacts/{contact_id}/promote",
            post(crate::gdrive::promote_contact),
        )
        .route(
            "/v1/control-plane/gdrive/contacts/{contact_id}/dismiss",
            post(crate::gdrive::dismiss_contact),
        )
        .route(
            "/v1/control-plane/gdrive/contacts/{contact_id}/qualify",
            post(crate::gdrive::qualify_contact),
        )
        .route(
            "/v1/control-plane/gdrive/files/{file_id}/audience",
            post(crate::gdrive::set_file_audience),
        )
        // ── Listing + representation (§4h-12) ─────────────────────────
        // The band's public-when-shared profile: the editor reads the whole
        // state, `save` writes the draft only, and `publish`/`unlist` are the
        // only visibility transitions. `rotate-token` revokes links already
        // sent — the token is the admission, so rotating it is the revoke.
        .route(
            "/v1/control-plane/listing",
            get(crate::band_listing::get_listing).post(crate::band_listing::put_listing),
        )
        .route(
            "/v1/control-plane/listing/publish",
            post(crate::band_listing::publish_listing),
        )
        .route(
            "/v1/control-plane/listing/unlist",
            post(crate::band_listing::unlist_listing),
        )
        .route(
            "/v1/control-plane/listing/rotate-token",
            post(crate::band_listing::rotate_listing_token),
        )
        // What to book next, and why. A refusal is part of the answer and
        // comes back 200 with its sentence: "nobody has played a room here"
        // tells the band what to go and find, and a 4xx would make the console
        // treat the most useful half of the output as a failure.
        .route(
            "/v1/control-plane/gig-plan",
            get(crate::gig_planning::band_gig_plan),
        )
        // Approving the proposal is the approval (4G.4): one action for the
        // whole room, queued rather than parked for a second yes on another
        // screen.
        .route(
            "/v1/control-plane/gig-plan/approve",
            post(crate::gig_planning::approve_gig_proposal),
        )
        // Audience attestations. The tenant decides whether a document exists
        // and who gets the link; it never decides what the document says, so
        // the issue body carries city slugs and nothing else. Revoking
        // withdraws without deleting the record of having issued, and rotating
        // mints a fresh link — revoke-by-rotation, same as the listing.
        .route(
            "/v1/control-plane/attestations",
            get(crate::attestation::list_attestations).post(crate::attestation::issue_attestation),
        )
        .route(
            "/v1/control-plane/attestations/{digest}/revoke",
            post(crate::attestation::revoke_attestation),
        )
        .route(
            "/v1/control-plane/attestations/{digest}/rotate",
            post(crate::attestation::rotate_attestation_link),
        )
        // Representation contacts are upserted here rather than through
        // the generic admin target route — this one pins the kind to
        // agent/label so it cannot be used to hide a press contact's
        // address from the band.
        .route(
            "/v1/control-plane/representation/targets",
            get(crate::band_listing::list_representation_targets)
                .post(crate::band_listing::upsert_representation_target),
        )
        .route(
            "/v1/control-plane/representation/approach",
            post(crate::band_listing::request_representation_approach),
        )
        // The booking-agent registry's own surface (§4h-10): the band sees
        // who the agents are and where the season door stands, asks for one
        // approach a season, and files what came back. The address never
        // leaves the platform — the send is brokered.
        .route(
            "/v1/control-plane/booking-agents",
            get(crate::booking_agents::list_booking_agents),
        )
        .route(
            "/v1/control-plane/booking-agents/approach",
            post(crate::booking_agents::request_booking_agent_approach),
        )
        // The batch form: the operator picks several agents off the
        // gate-state list and gets one card, not one card per agent.
        .route(
            "/v1/control-plane/booking-agents/approach-wave",
            post(crate::booking_agents::request_booking_agent_approach_wave),
        )
        .route(
            "/v1/control-plane/booking-agents/{agent_id}/reply",
            post(crate::booking_agents::record_booking_agent_reply),
        )
        .route(
            "/v1/control-plane/booking-agents/{agent_id}/reply-draft",
            post(crate::booking_agents::request_booking_agent_reply_draft),
        )
        // ── The shared night (§12-9) ────────────────────────────────
        // One read whose payload is the caller's resolved lens, and the
        // five writes around it: contribute, revoke, mint/revoke the
        // organiser link, and the billed act confirming itself. The lens
        // is never a parameter — the repository derives it from who is
        // asking, and a caller with no relationship gets the same 404 as
        // a night that does not exist.
        .route(
            "/v1/control-plane/nights/{place_event_id}",
            get(crate::night::get_night),
        )
        .route(
            "/v1/control-plane/nights/{place_event_id}/contributions",
            post(crate::night::upsert_night_contribution),
        )
        .route(
            "/v1/control-plane/nights/{place_event_id}/contributions/{kind}",
            axum::routing::delete(crate::night::revoke_night_contribution),
        )
        .route(
            "/v1/control-plane/nights/{place_event_id}/organiser-link",
            post(crate::night::mint_night_organiser_link)
                .delete(crate::night::revoke_night_organiser_link),
        )
        .route(
            "/v1/control-plane/nights/{place_event_id}/acts/{act_slug}/confirm",
            post(crate::night::confirm_night_act),
        )
        // "Stop asking me about this one." A per-action approval answers
        // whether one post may go out; this answers whether a target's posts
        // may, which is the decision an operator reaches after reading three
        // drafts from the same community. The list includes revoked and
        // expired grants on purpose — "which did we turn off, and when" is
        // the question asked after a community goes quiet.
        .route(
            "/v1/control-plane/autopilot/standing-approvals",
            post(crate::autopilot::grant_standing_approval)
                .get(crate::autopilot::list_standing_approvals),
        )
        .route(
            "/v1/control-plane/autopilot/standing-approvals/{action_kind}/{target_key}",
            axum::routing::delete(crate::autopilot::revoke_standing_approval),
        )
        .route(
            "/v1/control-plane/community-posts/{community_post_id}/register-manual",
            post(crate::fanbase::register_manual_community_post),
        )
        // The reply lane: the band's drafted answers to people who commented
        // on its Reddit posts — read, approve (as edited), or skip.
        .route(
            "/v1/control-plane/community-replies",
            get(crate::community_replies::list),
        )
        .route(
            "/v1/control-plane/community-replies/{reply_id}/approve",
            post(crate::community_replies::approve),
        )
        .route(
            "/v1/control-plane/community-replies/{reply_id}/skip",
            post(crate::community_replies::skip),
        )
        .route(
            "/v1/control-plane/social-posts/{social_post_id}/register-manual",
            post(crate::fanbase::register_manual_social_post),
        )
        .route(
            "/v1/control-plane/telegram-posts/{telegram_post_id}/register-manual",
            post(crate::fanbase::register_manual_telegram_post),
        )
        .route(
            "/v1/control-plane/discord-posts/{discord_post_id}/register-manual",
            post(crate::fanbase::register_manual_discord_post),
        )
        .route(
            "/v1/control-plane/webhook-endpoints",
            get(list_webhook_endpoints),
        )
        // ── Audience intelligence ──────────────────────────────────────
        // Fan list, fan detail, fan journey, fan tags, audience segments.
        // All reuse canonical admin handlers so this surface grows no
        // authority path of its own.
        .route(
            "/v1/control-plane/audience/overview",
            get(crate::audience::overview),
        )
        .route(
            "/v1/control-plane/audience/acquisition-sources",
            get(crate::audience::acquisition_sources),
        )
        .route(
            "/v1/control-plane/audience/city-funnel",
            get(crate::audience::city_funnel),
        )
        .route(
            "/v1/control-plane/audience/city-venues",
            get(crate::audience::city_venues),
        )
        // Console views: one read per page's first screen (console_views.rs).
        .route(
            "/v1/control-plane/views/cities/{city_slug}",
            get(crate::console_views::city_view),
        )
        .route(
            "/v1/control-plane/views/content-material",
            get(crate::console_views::content_material_view),
        )
        .route(
            "/v1/control-plane/audience/registry-verification-brief",
            get(crate::audience::registry_verification_brief),
        )
        .route(
            "/v1/control-plane/audience/fans",
            get(crate::audience::list_fans),
        )
        .route(
            "/v1/control-plane/audience/fans/{fan_id}",
            get(crate::audience::fan_detail),
        )
        .route(
            "/v1/control-plane/audience/fans/{fan_id}/journey",
            get(crate::audience::fan_journey),
        )
        .route(
            "/v1/control-plane/audience/fans/{fan_id}/tags",
            post(crate::audience::add_tag),
        )
        .route(
            "/v1/control-plane/audience/fans/{fan_id}/tags/{tag}/remove",
            post(crate::audience::remove_tag),
        )
        .route(
            "/v1/control-plane/audience/fans/{fan_id}/referral-code",
            post(crate::acquisition::admin_create_fan_referral_code),
        )
        .route(
            "/v1/control-plane/audience/segments",
            get(crate::audience::list_segments),
        )
        // P.1: the people the band already works with, with both roles
        // resolved — and the one-person, once-ever invitation.
        .route(
            "/v1/control-plane/contacts/dual-role",
            get(crate::latarnik_http::dual_role_contacts),
        )
        .route(
            "/v1/control-plane/contacts/{beacon_id}/latarnik-invite",
            post(crate::latarnik_http::invite_to_latarnik),
        )
        .route(
            "/v1/control-plane/contacts/{beacon_id}/latarnik-invite/preview",
            get(crate::latarnik_http::preview_invite),
        )
        .route(
            "/v1/control-plane/contacts/{beacon_id}/research",
            put(crate::latarnik_http::record_research),
        )
        .route(
            "/v1/control-plane/audience/segments/{slug}/preview",
            get(crate::audience::preview_segment),
        )
        // ── Growth metrics, objectives, posture ────────────────────────
        // Read-only coverage and trends; objective lifecycle; posture
        // read+write; acquisition channels; tour/show economics.
        .route(
            "/v1/control-plane/autopilot/growth-metrics/coverage",
            get(crate::autopilot::growth_metric_coverage),
        )
        .route(
            "/v1/control-plane/autopilot/growth-metrics/trends",
            get(crate::autopilot::growth_metric_trends),
        )
        .route(
            "/v1/control-plane/autopilot/reach-metrics",
            get(crate::autopilot::reach_metrics),
        )
        .route(
            "/v1/control-plane/autopilot/objectives",
            get(crate::autopilot::growth_objectives)
                .post(crate::autopilot::declare_growth_objective),
        )
        .route(
            "/v1/control-plane/autopilot/objectives/{objective_id}/retire",
            post(crate::autopilot::retire_growth_objective),
        )
        .route(
            "/v1/control-plane/autopilot/posture",
            get(crate::autopilot::growth_posture).post(crate::autopilot::set_growth_posture),
        )
        // Which executor lanes are live, blocked, or missing — the read-side
        // of the registry the dispatch gate enforces, so a missing capability
        // is a line on a screen instead of a refusal sentence at approve time.
        .route(
            "/v1/control-plane/autopilot/capabilities",
            get(crate::autopilot::executor_capabilities),
        )
        .route(
            "/v1/control-plane/autopilot/acquisition-channels",
            get(crate::autopilot::acquisition_channels),
        )
        .route(
            "/v1/control-plane/autopilot/tour-economics",
            get(crate::autopilot::tour_economics),
        )
        .route(
            "/v1/control-plane/autopilot/show-economics",
            get(crate::autopilot::show_economics),
        )
        .route(
            "/v1/control-plane/autopilot/chief-of-staff",
            get(crate::autopilot::chief_of_staff),
        )
        .route(
            "/v1/control-plane/autopilot/growth-envelope",
            get(crate::autopilot::growth_envelope).post(crate::autopilot::set_growth_envelope),
        )
        // ── Trusted material ──────────────────────────────────────────
        // The real-material panel: every fact the content loop may write
        // about. GET lists, POST upserts through the same versioned,
        // idempotent command the admin route uses.
        .route(
            "/v1/control-plane/autopilot/content-sources",
            get(crate::autopilot::list_content_sources)
                .post(crate::autopilot::upsert_content_source),
        )
        // The operator's "push this drop now": stamps the source's surge
        // request and wakes the worker — the next cycle fans out every lane
        // that has not already delivered. Re-promoting is idempotent: lanes
        // that already sent keep their dedupe keys and are not re-sent.
        .route(
            "/v1/control-plane/autopilot/content-sources/{source_id}/promote",
            post(crate::autopilot::promote_content_source),
        )
        // The content page's pipeline read: pending drafts, live material
        // count, and the titles those drafts cite — one narrow read model
        // instead of the cockpit-wide overview fan-out.
        .route(
            "/v1/control-plane/content/pipeline",
            get(crate::autopilot::content_pipeline),
        )
        // Which of the band's own posts held attention, against its own
        // medians — the feedback the next video is made from.
        .route(
            "/v1/control-plane/content/hooks",
            get(crate::ops::content_hooks),
        )
        .route(
            "/v1/control-plane/autopilot/content-suggestions/{suggestion_id}/outcome",
            post(crate::autopilot::report_suggestion_outcome),
        )
        // ── Outreach & booking discovery ──────────────────────────────
        // Candidate queues for the growth pipeline: what the agent found,
        // and the two decisions a human can make about one finding.
        .route(
            "/v1/control-plane/autopilot/outreach/candidates",
            get(crate::autopilot::list_outreach_candidates),
        )
        .route(
            "/v1/control-plane/autopilot/outreach/candidates/{candidate_id}/confirm",
            post(crate::autopilot::confirm_outreach_candidate),
        )
        // The CRM registry import: the screen's verdicts on every proposed
        // contact, and the bulk approval that turns the admitted ones into
        // outreach targets. Approval is the consent point.
        .route(
            "/v1/control-plane/autopilot/outreach/import-proposals",
            get(crate::autopilot::list_outreach_import_proposals),
        )
        .route(
            "/v1/control-plane/autopilot/outreach/import-proposals/approve",
            post(crate::autopilot::approve_outreach_import_proposals),
        )
        .route(
            "/v1/control-plane/autopilot/booking-discovery/candidates",
            get(crate::autopilot::list_booking_candidates),
        )
        .route(
            "/v1/control-plane/autopilot/booking-discovery/candidates/{candidate_id}/confirm",
            post(crate::autopilot::confirm_booking_candidate),
        )
        // ── Beacon signal network ─────────────────────────────────────
        // The press and industry relationship pipeline: who the agent is
        // talking to, what they asked for, and what the agent committed to.
        .route(
            "/v1/control-plane/autopilot/beacon-signal",
            get(crate::beacon_signal::admin_dashboard),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-signal/candidates",
            get(crate::beacon_signal::admin_candidates),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-press-requests",
            get(crate::beacon_signal::admin_press_requests),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-press-requests/{press_request_id}/resolve",
            post(crate::beacon_signal::admin_resolve_press_request),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-press-assets",
            get(crate::beacon_signal::admin_press_assets)
                .post(crate::beacon_signal::admin_upsert_press_asset),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-signal-engagements",
            get(crate::beacon_signal::admin_engagements),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-coverage",
            get(crate::beacon_signal::admin_coverage),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-network",
            // The action side was registered on `/v1/admin` only, so the
            // console could watch discovery and candidates and do nothing
            // about either. Importing researched contacts, approving a
            // candidate and queueing invites all arrive here.
            get(crate::beacon_signal::admin_beacon_network)
                .post(crate::beacon_signal::admin_beacon_network_action),
        )
        // ── Release campaigns ─────────────────────────────────────────
        .route(
            "/v1/control-plane/autopilot/beacon-release-campaigns",
            // Create was registered on `/v1/admin` and never here, so the
            // panel could list campaigns, launch them and close them — but the
            // only route that makes one was unreachable. Its own empty state
            // said "create one from the release plan"; no such surface existed.
            get(crate::beacon_signal::admin_list_release_campaigns)
                .post(crate::beacon_signal::admin_create_release_campaign),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-release-campaigns/{campaign_id}/launch",
            post(crate::beacon_signal::admin_launch_release_campaign),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-release-campaigns/{campaign_id}/close",
            post(crate::beacon_signal::admin_close_release_campaign),
        )
        .route(
            "/v1/control-plane/autopilot/beacon-release-campaigns/{campaign_id}/recipients",
            get(crate::beacon_signal::admin_list_release_recipients),
        )
        // ── Play ledger ───────────────────────────────────────────────
        // What the agent committed to, what it did, and what each number
        // is allowed to prove.
        .route(
            "/v1/control-plane/autopilot/plays",
            get(crate::autopilot::play_ledger),
        )
        // ── Beacon management ─────────────────────────────────────────
        // Beacons are the local-growth surface: people in a city who carry a
        // release or a show to an audience we do not own. Six read endpoints
        // were exposed and not one write, so the operator could watch the
        // roster and change nothing in it — no adding a beacon, no inviting
        // one, no recording that somebody replied.
        .route(
            "/v1/control-plane/autopilot/beacons",
            post(crate::autopilot::upsert_beacon),
        )
        .route(
            "/v1/control-plane/autopilot/beacons/signal-invites/batch",
            post(crate::beacon_signal::create_invite_batch),
        )
        .route(
            "/v1/control-plane/autopilot/beacons/{beacon_id}/signal-invites",
            post(crate::beacon_signal::create_invite),
        )
        .route(
            "/v1/control-plane/autopilot/beacons/{beacon_id}/signal-state",
            post(crate::beacon_signal::admin_set_state),
        )
        .route(
            "/v1/control-plane/autopilot/beacons/{beacon_id}/reply",
            post(crate::autopilot::record_beacon_reply),
        )
        // ── Audience graph ────────────────────────────────────────────
        // Where fans already gather. Until now the only way to register a
        // community was psql against the tenant's database, because the
        // capability existed solely under `/v1/admin` and nothing proxied it.
        //
        // `import` carries its own body limit: the router-wide 8 KiB is right
        // for the bounded mutations around it, but a scan of communities is a
        // list — the handler already caps it at MAX_IMPORT_PLACES, and a limit
        // below that cap would reject payloads the handler is built to accept.
        .route(
            "/v1/control-plane/audience-graph/places",
            get(crate::audience_graph::list_places).post(crate::audience_graph::upsert_place),
        )
        .route(
            "/v1/control-plane/audience-graph/places/import",
            post(crate::audience_graph::import_scan)
                .layer(DefaultBodyLimit::max(MAX_IMPORT_BODY_BYTES)),
        )
        // Tenant-uploaded media (join-ask screenshots first): the router's
        // 8 KiB JSON default is far under an image, so the route raises it
        // to the same ceiling Meta itself enforces on post images.
        .route(
            "/v1/control-plane/media",
            post(crate::media::upload_media)
                .layer(DefaultBodyLimit::max(crate::media::MAX_MEDIA_BODY_BYTES)),
        )
        // Route-local authentication is intentional. The global middleware
        // still separates AREA and management credentials, but this guard
        // makes adding a route here fail closed even if the global path matcher
        // or the private tunnel allowlist has not been updated yet.
        .route_layer(from_fn_with_state(state.clone(), require_control_plane))
        .layer(DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES))
        .with_state(state)
}

pub(crate) async fn require_control_plane(
    State(state): State<crate::AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !crate::security::bearer_sha256_matches_either(
        request.headers(),
        state.control_plane_api_key_sha256,
        state.previous_control_plane_api_key_sha256,
    ) {
        return crate::Problem::unauthorized(crate::request_id(request.headers()))
            .private()
            .into_response();
    }
    next.run(request).await
}

/// Read-only list of the tenant's configured outbound webhook endpoints.
/// The Control Plane surfaces these in its Notifiers tab so operators see
/// which CrowdRelay-owned delivery targets already exist before adding a
/// parallel notifier channel.
async fn list_webhook_endpoints(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = crate::request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match sqlx::query_as::<_, WebhookEndpointRow>(
        r#"
        SELECT id, name, url, active
        FROM webhook_endpoints
        WHERE workspace_id = $1
        ORDER BY created_at, name
        "#,
    )
    .bind(workspace_id)
    .fetch_all(&state.database)
    .await
    {
        Ok(rows) => {
            let items: Vec<WebhookEndpointSummary> = rows
                .into_iter()
                .map(|row| WebhookEndpointSummary {
                    id: row.id,
                    name: row.name,
                    url_host: url_host(&row.url),
                    active: row.active,
                })
                .collect();
            (
                [(CACHE_CONTROL, "private, no-store")],
                axum::Json(serde_json::json!({ "endpoints": items })),
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "control-plane webhook-endpoints read failed");
            crate::Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct WebhookEndpointRow {
    id: Uuid,
    name: String,
    url: String,
    active: bool,
}

#[derive(Debug, Serialize)]
struct WebhookEndpointSummary {
    id: Uuid,
    name: String,
    url_host: String,
    active: bool,
}

/// Reduce a full URL to its origin so the control plane never receives a
/// signed-webhook target path or query string.
fn url_host(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .map(|parsed| {
            format!(
                "{}://{}",
                parsed.scheme(),
                parsed.host_str().unwrap_or_default()
            )
        })
        .unwrap_or_else(|| raw.to_owned())
}
