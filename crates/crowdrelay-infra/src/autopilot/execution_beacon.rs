// The beacon cross-promotion executor, split out of `execution.rs`: it
// re-verifies the partner row, reserves the contact window, emits the outward
// send and records the campaign touch inside the caller's transaction.

/// Mints one redirect owned by the exact Beacon action.
///
/// Posts recover attribution through their publication ledger. Email has no
/// post row, so the link itself owns the action. That lets the ordinary click
/// + signup spine attribute a person to the named relationship without
/// inventing a parallel Beacon analytics system.
async fn ensure_beacon_action_link(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    beacon_id: crowdrelay_domain::BeaconId,
    site_root: Option<&str>,
    suffix: &str,
    destination: Option<&str>,
    creative: &str,
) -> Result<Option<String>, RepositoryError> {
    let Some(site_root) = site_root.map(str::trim).filter(|root| !root.is_empty()) else {
        return Ok(None);
    };
    let Some(destination) = destination
        .map(str::trim)
        .filter(|value| value.starts_with("https://") || value.starts_with("http://"))
    else {
        return Ok(None);
    };
    let slug = format!("beacon-{suffix}-{}", action_id.into_uuid().simple());
    sqlx::query(
        r#"
        INSERT INTO smart_links (
            workspace_id, slug, destination_url, active, action_id,
            channel_source, channel_community, channel_creative
        ) VALUES ($1,$2,$3,true,$4,'beacon',$5,$6)
        ON CONFLICT (workspace_id, slug) DO UPDATE
        SET destination_url=EXCLUDED.destination_url,
            active=true,
            action_id=EXCLUDED.action_id,
            channel_source=EXCLUDED.channel_source,
            channel_community=EXCLUDED.channel_community,
            channel_creative=EXCLUDED.channel_creative
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&slug)
    .bind(destination)
    .bind(action_id.into_uuid())
    .bind(beacon_id.to_string())
    .bind(creative)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(Some(format!("{}/l/{slug}", site_root.trim_end_matches('/'))))
}

/// Executes a beacon cross-promotion ask: re-verify the partner row under its
/// locked version, reserve the contact window, emit the outward send with
/// evidence, then record the campaign touch — all in the caller's transaction.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_beacon_outreach(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    beacon_id: crowdrelay_domain::BeaconId,
    event_id: EventId,
    beacon_version: i64,
    phase: &crowdrelay_domain::beacons::BeaconOutreachPhase,
    template_key: &str,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let target = sqlx::query_as::<_, (String, String, String, String, Option<String>, OffsetDateTime, String, Option<String>)>(
        r#"
        SELECT beacon.beacon_kind, beacon.display_name, beacon.contact_email,
               event.title, event.venue, event.starts_at, event.slug, event.ticket_url
        FROM beacons AS beacon
        JOIN events AS event
          ON event.workspace_id = beacon.workspace_id AND event.id = $3
        WHERE beacon.workspace_id = $1 AND beacon.id = $2
          AND beacon.version = $4
          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact
          AND beacon.contact_email IS NOT NULL
          AND event.status IN ('published','completed')
        FOR SHARE OF beacon, event
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(beacon_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(beacon_version)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    let beacon_kind = operations::parse_beacon_kind(&target.0)?;
    let allowed_offers = beacon_kind.offer_keys_for_phase(*phase);
    reserve_contact_window(
        transaction,
        workspace_id,
        action_id,
        "beacon",
        &target.2,
        now,
        false,
    )
    .await?;
    let city = sqlx::query_scalar::<_, Option<String>>(
        r#"
        SELECT city.name
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1 AND event.id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    // The show and press links are on the tenant's own site, but the
    // /pl/live/… and /pl/epk/ paths are the first tenant's layout — anyone
    // else's letter gets no links rather than another band's show page and
    // EPK. No site of its own means no links either — the letter can still
    // carry the event's ticket URL.
    let site_root = sqlx::query_scalar::<_, String>(
        "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'member_site_base_url'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .map(|value| value.trim().trim_end_matches('/').to_owned())
    .filter(|root| !root.is_empty());
    let (_, first_tenant) =
        crate::beacon_signal::beacon_release_signature(&mut **transaction, workspace_id.into_uuid())
            .await
            .map_err(map_sqlx)?;
    let direct_show_url = if first_tenant {
        site_root
            .as_deref()
            .map(|root| format!("{root}/pl/live/{}/", target.6))
    } else {
        None
    };
    let phase_key = match phase {
        crowdrelay_domain::beacons::BeaconOutreachPhase::Initial => "initial",
        crowdrelay_domain::beacons::BeaconOutreachPhase::CollaborationFollowUp => {
            "collaboration_follow_up"
        }
        crowdrelay_domain::beacons::BeaconOutreachPhase::LocalPush => "local_push",
        crowdrelay_domain::beacons::BeaconOutreachPhase::PostShowThanks => "post_show_thanks",
    };
    let show_url = ensure_beacon_action_link(
        transaction,
        workspace_id,
        action_id,
        beacon_id,
        site_root.as_deref(),
        "show",
        direct_show_url.as_deref(),
        phase_key,
    )
    .await?
    .or(direct_show_url);
    let ticket_url = ensure_beacon_action_link(
        transaction,
        workspace_id,
        action_id,
        beacon_id,
        site_root.as_deref(),
        "ticket",
        target.7.as_deref(),
        phase_key,
    )
    .await?
    .or_else(|| target.7.clone());
    let epk_url = if first_tenant {
        site_root.as_deref().map(|root| format!("{root}/pl/epk/"))
    } else {
        None
    };
    emit_outward_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.beacon.outreach_requested",
        format!("beacon:{beacon_id}"),
        "verified beacon, accepts outreach — cross-promotion for the band's event",
        json!({
            "action_id": action_id,
            "beacon_id": beacon_id,
            "beacon_kind": target.0,
            "beacon_name": target.1,
            "contact_email": target.2,
            "event": {
                "id": event_id,
                "title": target.3,
                "venue": target.4,
                "city": city,
                "starts_at": crowdrelay_domain::wire_time::Wire(&target.5),
                "slug": target.6,
                "ticket_url": ticket_url,
                "show_url": show_url,
            },
            "phase": phase,
            "template_key": template_key,
            "personalization_contract": {
                "local_reason_required": true,
                "human_tone": true,
                "allowed_offers": allowed_offers,
                "epk_url": epk_url,
                "single_primary_ask": true,
                "use_event_ticket_url_when_cta_is_relevant": true,
                "use_verified_press_or_live_proof_only": true,
                "never_invent_local_connection_or_editorial_interest": true,
            },
        }),
    )
    .await?;
    sqlx::query(
        r#"
        INSERT INTO beacon_campaigns (
            workspace_id, beacon_id, event_id, status, last_phase,
            last_outreach_at, followup_count
        ) VALUES ($1,$2,$3,'contacted',$4,$5,1)
        ON CONFLICT (workspace_id, beacon_id, event_id) DO UPDATE
        SET status = CASE
                WHEN beacon_campaigns.status IN ('interested','partner')
                THEN beacon_campaigns.status
                ELSE 'contacted'
            END,
            last_phase = EXCLUDED.last_phase,
            last_outreach_at = EXCLUDED.last_outreach_at,
            followup_count = beacon_campaigns.followup_count + 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(beacon_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(phase_key)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

