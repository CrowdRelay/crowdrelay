// The beacon cross-promotion executor, split out of `execution.rs`: it
// re-verifies the partner row, reserves the contact window, emits the outward
// send and records the campaign touch inside the caller's transaction.
//
// `prepare_*` carries everything up to the emission; the executor arm emits
// to the outbox, and the operator lane (`beacon_lane.rs`) uses the same
// preparation to hand the ask to a person when no executor advertises the
// capability. The guards a partner earns — verified, accepting outreach, not
// declined, contact window reserved, campaign touch recorded — are the same
// either way: who carries the words is the only difference.

/// Mints one redirect owned by the exact Beacon action.
///
/// Posts recover attribution through their publication ledger. Email has no
/// post row, so the link itself owns the action. That lets the ordinary click
/// + signup spine attribute a person to the named relationship without
///   inventing a parallel Beacon analytics system.
#[allow(clippy::too_many_arguments)]
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

/// The tenant's public site root plus whether the tenant is the first tenant
/// whose `/pl/live/…` and `/pl/epk/` layout the link builders assume.
/// `None`/false means no tenant-native links — the letter falls back to the
/// event's own ticket URL rather than pointing a stranger at another band's
/// page.
async fn beacon_letter_site(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
) -> Result<(Option<String>, bool), RepositoryError> {
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
    Ok((site_root, first_tenant))
}

/// What `prepare_beacon_outreach` built: the verified partner facts, the
/// tracked links, and the payload the outward emission carries. The operator
/// lane reads the fields for the letter; the executor arm emits `payload`.
pub(super) struct BeaconOutreachDispatch {
    pub beacon_name: String,
    pub contact_email: String,
    pub phase_key: &'static str,
    pub event_title: String,
    pub event_venue: Option<String>,
    pub event_city: Option<String>,
    pub event_starts_at: OffsetDateTime,
    pub show_url: Option<String>,
    pub ticket_url: Option<String>,
    pub epk_url: Option<String>,
    pub payload: Value,
}

/// Re-verifies the partner row under its locked version, reserves the contact
/// window, records the campaign touch, and mints the tracked links — the whole
/// pre-emission contract the executor arm then emits from.
///
/// `record_touch` distinguishes "the send is happening now" from "the facts
/// are being rebuilt for a letter": the executor and the operator's first
/// `prepare` pass `true`; a re-prepare or an emitted-but-undelivered action
/// passes `false`, because its dispatch already counted the touch and
/// `followup_count` feeds the next cycle's decision keys — inflating it
/// re-fires proposals that were already made.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_beacon_outreach(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    beacon_id: crowdrelay_domain::BeaconId,
    event_id: EventId,
    beacon_version: i64,
    phase: &crowdrelay_domain::beacons::BeaconOutreachPhase,
    template_key: &str,
    record_touch: bool,
    now: OffsetDateTime,
) -> Result<BeaconOutreachDispatch, RepositoryError> {
    let target = sqlx::query_as::<_, (String, String, String, String, Option<String>, OffsetDateTime, String, Option<String>)>(
        r#"
        SELECT beacon.beacon_kind, beacon.display_name, beacon.contact_email,
               event.title, event.venue, event.starts_at, event.slug, event.ticket_url
        FROM beacons AS beacon
        JOIN events AS event
          ON event.workspace_id = beacon.workspace_id AND event.id = $3
        LEFT JOIN beacon_campaigns AS campaign
          ON campaign.workspace_id = beacon.workspace_id
         AND campaign.beacon_id = beacon.id
         AND campaign.event_id = event.id
        WHERE beacon.workspace_id = $1 AND beacon.id = $2
          AND beacon.version = $4
          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact
          AND beacon.contact_email IS NOT NULL
          AND event.status IN ('published','completed')
          -- The ask travels with the answer the operator gave at approval
          -- time; hours can pass before the send runs, and a deferred or
          -- declined pair must not ship on the stale yes.
          AND COALESCE(campaign.status, 'candidate') NOT IN ('declined','suppressed','closed')
          AND (campaign.deferred_until IS NULL OR campaign.deferred_until <= $5)
        -- `campaign` stays out of the lock list: FOR SHARE rejects the
        -- nullable side of an outer join, and the guarded upsert below is
        -- the durable half of this guard anyway.
        FOR SHARE OF beacon, event
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(beacon_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(beacon_version)
    .bind(now)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    let beacon_kind = operations::parse_beacon_kind(&target.0)?;
    let allowed_offers = beacon_kind.offer_keys_for_phase(*phase);
    if record_touch {
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
    }
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
    let (site_root, first_tenant) = beacon_letter_site(transaction, workspace_id).await?;
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
    // Claim the pair before anything is built on the approval: the
    // re-check above has no campaign row to lock when the pair is new, so
    // a defer/decline or partner reply committing between the check and
    // this write would otherwise be stomped by the send. The guarded
    // upsert is the check made durable — it inserts or locks the row, and
    // an answer that landed first makes it return nothing.
    if record_touch {
        sqlx::query_scalar::<_, String>(
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
            -- A sent ask is no longer deferred, and the new status is never
            -- 'declined', so the decline provenance has to clear or the
            -- CHECK forbids the row.
            deferred_until = NULL,
            declined_via = NULL,
            followup_count = beacon_campaigns.followup_count + 1
        WHERE beacon_campaigns.status NOT IN ('declined','suppressed','closed')
          AND (beacon_campaigns.deferred_until IS NULL
               OR beacon_campaigns.deferred_until <= $5)
        RETURNING status
        "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(beacon_id.into_uuid())
        .bind(event_id.into_uuid())
        .bind(phase_key)
        .bind(now)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::Conflict)?;
    }
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
    let payload = json!({
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
    });
    Ok(BeaconOutreachDispatch {
        beacon_name: target.1,
        contact_email: target.2,
        phase_key,
        event_title: target.3,
        event_venue: target.4,
        event_city: city,
        event_starts_at: target.5,
        show_url,
        ticket_url,
        epk_url,
        payload,
    })
}

/// Executes a beacon cross-promotion ask end to end: the shared preparation,
/// then the outward emission — the executor path.
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
    let dispatch = prepare_beacon_outreach(
        transaction,
        workspace_id,
        action_id,
        beacon_id,
        event_id,
        beacon_version,
        phase,
        template_key,
        true,
        now,
    )
    .await?;
    emit_outward_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.beacon.outreach_requested",
        format!("beacon:{beacon_id}"),
        "verified beacon, accepts outreach — cross-promotion for the band's event",
        dispatch.payload,
    )
    .await
}

/// Same split for the invite batch: re-verify under the decision-time guards
/// and build the ask payload; the caller decides whether the payload leaves
/// through the outbox or goes into an operator's hands.
pub(super) struct BeaconInviteDispatch {
    pub beacon_name: String,
    pub contact_email: String,
    pub requested_count: u16,
    pub event_title: String,
    pub event_slug: String,
    pub event_starts_at: OffsetDateTime,
    pub payload: Value,
}

/// Re-reads the beacon under the same guards the rule used at decision time:
/// hours passed since then, and a partner who went cold or a show that moved
/// must not receive yesterday's ask. Returns the facts and the emission
/// payload; sends nothing.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_beacon_invite_batch(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    beacon_id: crowdrelay_domain::BeaconId,
    beacon_version: i64,
    event_id: EventId,
    requested_count: u16,
    now: OffsetDateTime,
) -> Result<BeaconInviteDispatch, RepositoryError> {
    let target = sqlx::query_as::<_, (String, String, String, String, OffsetDateTime)>(
        r#"
        SELECT beacon.display_name, beacon.contact_email,
               event.title, event.slug, event.starts_at
        FROM beacons AS beacon
        JOIN events AS event
          ON event.workspace_id = beacon.workspace_id AND event.id = $3
        LEFT JOIN beacon_campaigns AS campaign
          ON campaign.workspace_id = beacon.workspace_id
         AND campaign.beacon_id = beacon.id
         AND campaign.event_id = event.id
        WHERE beacon.workspace_id = $1 AND beacon.id = $2
          AND beacon.version = $4
          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact
          AND beacon.contact_email IS NOT NULL
          AND event.status = 'published'
          -- Same stale-yes guard as the outreach send: an
          -- answer the booker wrote after approval must gate
          -- the batch too.
          AND COALESCE(campaign.status, 'candidate') NOT IN ('declined','suppressed','closed')
          AND (campaign.deferred_until IS NULL OR campaign.deferred_until <= $5)
        -- `campaign` is the nullable side of the LEFT JOIN — FOR SHARE
        -- refuses to lock it, which is why this statement never ran.
        FOR SHARE OF beacon, event
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(beacon_id.into_uuid())
    .bind(event_id.into_uuid())
    .bind(beacon_version)
    .bind(now)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    let payload = json!({
        "action_id": action_id,
        "beacon_id": beacon_id,
        "beacon_version": beacon_version,
        "event_id": event_id,
        "requested_count": requested_count,
        "beacon_name": target.0,
        "contact_email": target.1,
        "event": {
            "title": target.2,
            "slug": target.3,
            "starts_at": crowdrelay_domain::wire_time::Wire(&target.4),
        },
        "callback_path": "/v1/admin/beacons",
        "invite_contract": {
            "codes_issued_by_crowdrelay": true,
            "never_purchase_or_bot_invites": true,
            "only_their_own_community": true,
            "one_batch_per_beacon_per_show": true
        }
    });
    Ok(BeaconInviteDispatch {
        beacon_name: target.0,
        contact_email: target.1,
        requested_count,
        event_title: target.2,
        event_slug: target.3,
        event_starts_at: target.4,
        payload,
    })
}

/// The executor path for an invite batch: the shared preparation, then the
/// outward emission. The ask travels; the codes stay here — invite codes are
/// issued by the workspace's own machinery when the partner answers yes, so
/// every signup they produce is attributed and consented by construction —
/// neither the executor nor the partner invents either.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_beacon_invite_batch(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    beacon_id: crowdrelay_domain::BeaconId,
    beacon_version: i64,
    event_id: EventId,
    requested_count: u16,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let dispatch = prepare_beacon_invite_batch(
        transaction,
        workspace_id,
        action_id,
        beacon_id,
        beacon_version,
        event_id,
        requested_count,
        now,
    )
    .await?;
    emit_outward_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.beacon.invite_batch_requested",
        format!("beacon:{beacon_id}"),
        "verified beacon partner, accepts outreach — invite batch for its community",
        dispatch.payload,
    )
    .await
}
