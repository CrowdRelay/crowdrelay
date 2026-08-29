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
    routing::{get, post},
};
use serde::Serialize;
use uuid::Uuid;

const MAX_CONTROL_BODY_BYTES: usize = 8 * 1024;

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
        .route(
            "/v1/control-plane/autopilot/scorecard",
            get(crate::autopilot::scorecard_handler),
        )
        .route(
            "/v1/control-plane/autopilot/reply-triage",
            get(crate::autopilot::reply_triage_handler),
        )
        .route(
            "/v1/control-plane/autopilot/actions/{action_id}/approve",
            post(crate::autopilot::approve_action),
        )
        .route(
            "/v1/control-plane/autopilot/actions/{action_id}/cancel",
            post(crate::autopilot::cancel_action),
        )
        .route(
            "/v1/control-plane/autopilot/decisions/{decision_id}/handled-externally",
            post(crate::autopilot::mark_decision_handled_externally),
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
            get(crate::portfolio::list_amplification),
        )
        .route(
            "/v1/control-plane/portfolio/amplification/{consent_id}/decide",
            post(crate::portfolio::decide_amplification),
        )
        .route(
            "/v1/control-plane/tenant-settings",
            get(crate::tenant_settings_http::get_brand_settings),
        )
        .route(
            "/v1/control-plane/tenant-settings/{key}",
            post(crate::tenant_settings_http::upsert_setting),
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
            "/v1/control-plane/fanbases/connections/oauth/{platform}/start",
            post(crate::fanbase::start_fanbase_oauth),
        )
        .route(
            "/v1/control-plane/fanbases/connections/oauth/{platform}/callback",
            post(crate::fanbase::fanbase_oauth_callback),
        )
        .route(
            "/v1/control-plane/fanbases/connections/{connection_id}",
            axum::routing::delete(crate::fanbase::delete_fanbase_connection),
        )
        .route(
            "/v1/control-plane/community-posts/{community_post_id}/register-manual",
            post(crate::fanbase::register_manual_community_post),
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
            post(crate::autopilot::set_growth_envelope),
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
            get(crate::beacon_signal::admin_press_assets),
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
            get(crate::beacon_signal::admin_beacon_network),
        )
        // ── Release campaigns ─────────────────────────────────────────
        .route(
            "/v1/control-plane/autopilot/beacon-release-campaigns",
            get(crate::beacon_signal::admin_list_release_campaigns),
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
        // Route-local authentication is intentional. The global middleware
        // still separates AREA and management credentials, but this guard
        // makes adding a route here fail closed even if the global path matcher
        // or the private tunnel allowlist has not been updated yet.
        .route_layer(from_fn_with_state(state.clone(), require_control_plane))
        .layer(DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES))
        .with_state(state)
}

async fn require_control_plane(
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
