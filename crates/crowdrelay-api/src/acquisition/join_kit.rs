// The join kit (FAN_100 §B4): one tracked link per standing social
// placement — the bio, the pinned comment, the channel description. The
// weekly join-ask rotates a post through its own `cta_url`; these are the
// permanent slots a post never reaches, pasted once by the band and left.
//
// `POST` materializes the canonical set against the tenant's `/signal`
// page — idempotent on slug, so re-posting repairs a stale destination
// rather than failing. `GET` is the weekly readout: clicks from the links'
// own rows, signups and D30 retention joined from the channel readout the
// placements attribute under, so a placement reads the same way a channel
// does. The links themselves are what `untracked_links_in` would flag if
// pasted raw — minting them here is what keeps that gate green.

use crowdrelay_application::autopilot::AutopilotControlRepository;
use crowdrelay_domain::acquisition_channel::ChannelAttribution;
use crowdrelay_domain::join_kit::{JOIN_KIT_PLACEMENTS, join_kit_destination};
use crowdrelay_infra::tenant_settings::TenantSettingsRepository;

/// One placement's row in the kit: the canonical slot, the link when it has
/// been minted, and the readout numbers.
#[derive(Debug, Serialize)]
struct JoinKitPlacementView {
    platform: &'static str,
    placement: &'static str,
    slug: &'static str,
    hint: &'static str,
    /// The printable `{site}/l/{slug}` URL — `null` until the kit has been
    /// materialized or the tenant has no site root.
    link_url: Option<String>,
    /// Where the link lands, when minted.
    destination_url: Option<String>,
    minted: bool,
    clicks_total: u32,
    clicks_7d: u32,
    /// Fans whose last pre-signup click was this placement's channel — the
    /// same last-click-wins rule the acquisition-channel readout applies,
    /// read from it rather than re-derived here.
    signups: u32,
    /// Of those, the fans still active, consented and meaningful inside
    /// thirty days — the "stayed" number the channel readout reports.
    activated_30d: u32,
}

#[derive(Debug, Serialize)]
struct JoinKitResponse {
    site_root: Option<String>,
    placements: Vec<JoinKitPlacementView>,
}

/// `GET /v1/admin/join-kit` — the placement checklist plus readout.
pub async fn admin_join_kit(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.acquisition.workspace_id();
    match join_kit_view(&state, workspace_id, request_id_value.clone()).await {
        Ok(view) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(view),
        )
            .into_response(),
        Err(problem) => problem.into_response(),
    }
}

/// `POST /v1/admin/join-kit` — materialize the canonical placement links.
///
/// Refuses when the tenant has no member site: a link with no destination
/// is a printed 404, the same refusal the install ask makes.
pub async fn admin_ensure_join_kit(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.acquisition.workspace_id();
    let brand = match TenantSettingsRepository::new(state.database.clone())
        .brand_settings(workspace_id.into_uuid())
        .await
    {
        Ok(brand) => brand,
        Err(error) => {
            tracing::warn!(%error, "join-kit brand settings load failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(site_root) = brand.site_root().map(str::to_owned) else {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    };

    for placement in JOIN_KIT_PLACEMENTS {
        let Ok(slug) = SmartLinkSlug::parse(placement.slug) else {
            tracing::error!(slug = placement.slug, "join-kit placement slug failed its own parse");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        };
        let destination = join_kit_destination(&site_root, placement);
        let command = UpsertSmartLinkCommand {
            workspace_id,
            slug: &slug,
            destination_url: &destination,
            channel_source: Some(placement.platform),
            channel_community: Some(placement.placement),
            channel_creative: None,
            campaign_id: None,
        };
        if let Err(error) = state
            .acquisition
            .acquisition_repository()
            .upsert_smart_link(&command)
            .await
        {
            tracing::warn!(%error, slug = placement.slug, "join-kit link upsert failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    }

    match join_kit_view(&state, workspace_id, request_id_value.clone()).await {
        Ok(view) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(view),
        )
            .into_response(),
        Err(problem) => problem.into_response(),
    }
}

/// Assembles the kit view: the canonical placements left joined onto the
/// minted links, their click tallies, and the channel readout's per-
/// placement signup/retention numbers.
async fn join_kit_view(
    state: &crate::AppState,
    workspace_id: WorkspaceId,
    request_id_value: Option<String>,
) -> Result<JoinKitResponse, Problem> {
    let brand = TenantSettingsRepository::new(state.database.clone())
        .brand_settings(workspace_id.into_uuid())
        .await
        .map_err(|error| {
            tracing::warn!(%error, "join-kit brand settings load failed");
            Problem::service_unavailable(request_id_value.clone()).private()
        })?;
    let site_root = brand.site_root().map(str::to_owned);
    let kit_slugs: Vec<String> = JOIN_KIT_PLACEMENTS
        .iter()
        .map(|placement| placement.slug.to_owned())
        .collect();

    let (links, stats, channels) = tokio::join!(
        state
            .acquisition
            .acquisition_repository()
            .list_smart_links(workspace_id),
        state
            .acquisition
            .acquisition_repository()
            .link_click_stats(workspace_id, &kit_slugs, OffsetDateTime::now_utc()),
        state
            .autopilot
            .load_acquisition_channels(workspace_id, OffsetDateTime::now_utc()),
    );
    let links = links.map_err(|error| {
        tracing::warn!(%error, "join-kit link listing failed");
        Problem::service_unavailable(request_id_value.clone()).private()
    })?;
    let stats = stats.map_err(|error| {
        tracing::warn!(%error, "join-kit click stats failed");
        Problem::service_unavailable(request_id_value.clone()).private()
    })?;
    let channels = channels.map_err(|error| {
        tracing::warn!(%error, "join-kit channel readout failed");
        Problem::service_unavailable(request_id_value.clone()).private()
    })?;

    let placements = JOIN_KIT_PLACEMENTS
        .iter()
        .map(|placement| {
            let link = links
                .iter()
                .find(|link| link.slug.as_str() == placement.slug);
            let stat = stats
                .iter()
                .find(|stat| stat.slug == placement.slug);
            // The channel row the link attributes under — (source,
            // community), no creative. A placement with no attributed fans
            // reads as zero, not as a missing row.
            let channel = channels.channels.iter().find(|row| {
                matches!(
                    &row.attribution,
                    ChannelAttribution::Attributed(identity)
                        if identity.source == placement.platform
                            && identity.community.as_deref() == Some(placement.placement)
                            && identity.creative.is_none()
                )
            });
            JoinKitPlacementView {
                platform: placement.platform,
                placement: placement.placement,
                slug: placement.slug,
                hint: placement.hint,
                link_url: link.and_then(|link| {
                    site_root
                        .as_deref()
                        .map(|site| format!("{}/l/{}", site.trim_end_matches('/'), link.slug.as_str()))
                }),
                destination_url: link.map(|link| link.destination_url.clone()),
                minted: link.is_some(),
                clicks_total: stat.map_or(0, |stat| stat.clicks_total),
                clicks_7d: stat.map_or(0, |stat| stat.clicks_7d),
                signups: channel.map_or(0, |channel| channel.signups),
                activated_30d: channel.map_or(0, |channel| channel.activated_30d),
            }
        })
        .collect();

    Ok(JoinKitResponse {
        site_root,
        placements,
    })
}
