//! One tracked link per (partner, event) for the scout pilot.
//!
//! The pilot hands each approved local partner a link that is theirs alone:
//! `smart_links.channel_source='beacon_partner'`, `channel_creative` the
//! beacon id, `channel_community` the event's city. Click, signup, activation
//! and retained-fan attribution all flow through the same smart-link
//! machinery every other channel already uses — no second attribution
//! system.
//!
//! Slugs are deterministic (`bp-{beacon8}-{event8}`), so a replayed request
//! returns the same link instead of minting a second one — clicks keep
//! accumulating on the URL the partner already has.
//!
//! Minting requires the beacon to have passed operator approval
//! (`verified AND accepts_outreach AND NOT do_not_contact`). A tracked link
//! is a launched action: it exists so the operator can hand it to a real
//! partner, and an unvetted candidate must not get one.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{OperatorActionRecord, record_operator_action};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartnerLinkMinted {
    pub beacon_id: Uuid,
    pub event_id: Option<Uuid>,
    pub slug: String,
    /// `/l/{slug}` — the path the operator hands to the partner. The public
    /// site base is a deployment property the API layer knows and infra does
    /// not, so the full URL is assembled by the caller.
    pub path: String,
    pub destination_url: String,
    /// True when the link already existed — the operator re-requested a link
    /// for a partner that already has one.
    pub already_existed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartnerLinkRefusal {
    /// No active beacon row for this workspace.
    BeaconNotFound,
    /// The beacon exists but has not passed operator approval
    /// (`verified AND accepts_outreach AND NOT do_not_contact`).
    BeaconNotApproved,
    /// `event_id` was given but no such event exists in the workspace.
    EventNotFound,
    /// `destination_url` is required when no `event_id` is given.
    DestinationRequired,
}

/// Mint (or fetch) the partner's tracked link in one transaction.
///
/// `event_page_base` is the tenant's public events root, e.g.
/// `https://virya.example/events`; it is used to build the default
/// destination (`{event_page_base}/{event.slug}`) when the operator does not
/// pass an explicit `destination_url`.
#[allow(clippy::too_many_arguments)]
pub async fn mint_partner_link(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    event_id: Option<Uuid>,
    destination_url: Option<&str>,
    event_page_base: &str,
    idempotency_key: &str,
    request_id: Option<&str>,
) -> Result<Result<PartnerLinkMinted, PartnerLinkRefusal>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Exactly one of the three beacon states: not found, found-not-approved,
    // found-approved. Reading all gate columns keeps the refusal honest
    // instead of collapsing "no such beacon" and "not approved yet".
    let beacon = sqlx::query_as::<_, (bool, bool, bool, String, String)>(
        r#"
        SELECT beacon.verified, beacon.accepts_outreach, beacon.do_not_contact,
               beacon.display_name,
               COALESCE(city.name, '') AS city_name
        FROM beacons beacon
        LEFT JOIN cities city ON city.id = beacon.city_id
        WHERE beacon.workspace_id = $1 AND beacon.id = $2 AND beacon.active
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((verified, accepts_outreach, do_not_contact, display_name, city_name)) = beacon else {
        return Ok(Err(PartnerLinkRefusal::BeaconNotFound));
    };
    if !(verified && accepts_outreach && !do_not_contact) {
        return Ok(Err(PartnerLinkRefusal::BeaconNotApproved));
    }

    let event = match event_id {
        Some(event_id) => {
            let row = sqlx::query_as::<_, (String, String)>(
                r#"
                SELECT slug, title FROM events
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(event_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                return Ok(Err(PartnerLinkRefusal::EventNotFound));
            };
            Some((event_id, row.0, row.1))
        }
        None => None,
    };

    let destination = match destination_url {
        Some(url) => {
            // Operator-supplied destinations must be https — they point the
            // partner's audience somewhere arbitrary.
            let url = url.trim();
            if !url.starts_with("https://") || url.len() > 2048 {
                return Ok(Err(PartnerLinkRefusal::DestinationRequired));
            }
            url.to_owned()
        }
        None => match &event {
            // The tenant's own event page is deployment config, not operator
            // input — plain http is legal for local/test bases.
            Some((_, slug, _)) => format!("{}/{slug}", event_page_base.trim_end_matches('/')),
            None => return Ok(Err(PartnerLinkRefusal::DestinationRequired)),
        },
    };

    let event_uuid = event.as_ref().map(|(id, _, _)| *id);
    // Deterministic slug: same partner + same event = same link, so a retry
    // or a second request returns the URL already handed out.
    let slug = match event_uuid {
        Some(event_id) => {
            format!("bp-{}-{}", beacon_id.simple(), event_id.simple())
        }
        None => format!("bp-{}-x", beacon_id.simple()),
    };
    // `bp-{32 hex}-{32 hex|x}` is 68 chars of ASCII — the slug CHECK's
    // 128-char ceiling can never be reached.
    let slug = slug.as_str();

    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO smart_links
            (id, workspace_id, slug, destination_url, active,
             channel_source, channel_community, channel_creative)
        VALUES ($1, $2, $3, $4, true, 'beacon_partner', $5, $6)
        ON CONFLICT (workspace_id, slug) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(slug)
    .bind(&destination)
    // channel_community is the event city when we have one, else the
    // partner's own city — the pilot reads it as "which local scene".
    .bind(if city_name.is_empty() {
        &display_name
    } else {
        &city_name
    })
    .bind(beacon_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    let already_existed = inserted.is_none();
    if already_existed {
        // Repoint the destination if the operator passed a different one —
        // the link is the partner's, what it points at is the campaign's.
        sqlx::query(
            r#"
            UPDATE smart_links
            SET destination_url = $3, active = true, updated_at = now()
            WHERE workspace_id = $1 AND slug = $2
              AND destination_url <> $3
            "#,
        )
        .bind(workspace_id)
        .bind(slug)
        .bind(&destination)
        .execute(&mut *tx)
        .await?;
    }

    // Record the link on the beacon row so the review surface can show it
    // without a second lookup. Append into `partner_links` — additive like
    // every other scout-era metadata write, never a replace.
    sqlx::query(
        r#"
        UPDATE beacons
        SET metadata = metadata || jsonb_build_object(
                'partner_links',
                COALESCE(metadata->'partner_links', '[]'::jsonb)
                    || jsonb_build_array(
                        jsonb_build_object(
                            'slug', $3::text,
                            'event_id', $4::text,
                            'destination_url', $5::text,
                            'minted_at', now()
                        )
                    )
            ),
            updated_at = now()
        WHERE workspace_id = $1 AND id = $2
          AND NOT (COALESCE(metadata->'partner_links', '[]'::jsonb)
                   @> jsonb_build_array(
                       jsonb_build_object('slug', $3::text, 'event_id', $4::text)))
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(slug)
    .bind(event_uuid.map(|id| id.to_string()))
    .bind(&destination)
    .execute(&mut *tx)
    .await?;

    record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "beacon_partner_link",
            target_type: "beacon",
            target_id: beacon_id,
            idempotency_key,
            request_id,
            details: serde_json::json!({
                "slug": slug,
                "event_id": event_uuid,
                "destination_url": destination,
                "already_existed": already_existed,
                "minted_at": OffsetDateTime::now_utc(),
            }),
        },
    )
    .await?;

    tx.commit().await?;
    Ok(Ok(PartnerLinkMinted {
        beacon_id,
        event_id: event_uuid,
        slug: slug.to_owned(),
        path: format!("/l/{slug}"),
        destination_url: destination,
        already_existed,
    }))
}
