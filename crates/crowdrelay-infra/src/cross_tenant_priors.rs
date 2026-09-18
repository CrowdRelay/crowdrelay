//! Cross-tenant priors (P.6): the aggregate record a promoter or a room
//! carries across every tenant on the platform.
//!
//! The registries are global — a promoter who booked three tenants is one
//! `place_counterparties` row, a room that hosted five is one `place_venues`
//! row — but the outcome history lives in workspace-scoped tables. The prior
//! is the only place the two meet, and it meets anonymously: every number
//! below is a `count(DISTINCT workspace_id)`, a count of tenants, never the
//! ids themselves. "Two of the three tenants who wrote got an answer" is the
//! whole of what a tenant learns; which tenants those were is nobody's
//! business, including theirs.
//!
//! The reads are deliberately unscoped — crossing workspaces is the feature,
//! not an oversight. The workspace-scope ratchet passes this file because
//! every statement still names `workspace_id` (as the aggregation column) —
//! there is no baseline entry and no guard beyond this paragraph. Anything
//! added here must keep the same shape: aggregates over `workspace_id`,
//! never per-tenant rows.
//!
//! The counts are exact at small cohorts: `tenants_contacted = 1` asserts
//! precisely one tenant on the platform mailed the address, and a reader who
//! knows their own ledger can tell whether that tenant was them. That
//! thin-cohort inference is the product — the registry badge already reveals
//! a relationship exists, and the prior is the measured record of how it
//! went. Keep it counts-only; never widen what a tenant can learn.

use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

/// One address's cross-tenant reply record — counts only.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct CounterpartyPrior {
    /// Distinct tenants whose records show a send to this address — an
    /// opportunity submission requested-or-confirmed, a mailed outreach or
    /// booking target, an agent approach, a beacon campaign touch, or a row
    /// in the contact-touches send ledger, whichever the tenant's ledgers
    /// kept.
    pub tenants_contacted: i64,
    /// Of those, the distinct tenants a reply of any disposition came back
    /// to. A decline is still a reply — the prior measures who answers, not
    /// who says yes.
    pub tenants_replied: i64,
    /// Of those, the distinct tenants whose thread ended in a win — an
    /// opportunity reaching `won`, or a reply the tenant marked positive.
    pub tenants_won: i64,
}

/// One room's cross-tenant play record — counts only.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct VenuePrior {
    /// Distinct tenants with a mark on this room — the acts that played it.
    pub tenants_played: i64,
    /// Total marks — the shows the room has hosted on record.
    pub shows: i64,
}

/// The reply record for each address in `email_keys`, across every
/// workspace. Keys are normalized the way `place_counterparties.email_key`
/// is — lower, trimmed — and callers pass staged contact addresses, event
/// counterparty mails, and booking-target addresses through the same
/// normalization the registry trigger applies.
///
/// A send is: an opportunity submission requested-or-confirmed naming the
/// address, a mailed outreach or booking target, an agent approach that
/// wrote an outbound interaction, a beacon campaign with `last_outreach_at`,
/// or a row in the send ledger — the last catches the sends (latarnik
/// invites, T+7 reports) the typed tables miss. A reply is an opportunity
/// that reached `replied`/`won`/`lost`, or an inbound row in whichever
/// interaction ledger the thread used — never the denormalized disposition
/// columns, which a suppression stamp can write without a reply. Nothing
/// here reads message content — the ledger is statuses and timestamps.
pub async fn counterparty_priors(
    pool: &PgPool,
    email_keys: &[String],
) -> Result<HashMap<String, CounterpartyPrior>, sqlx::Error> {
    if email_keys.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (String, i64, i64, i64)>(
        r#"
        SELECT email_key,
               count(DISTINCT workspace_id) AS tenants_contacted,
               count(DISTINCT workspace_id) FILTER (WHERE replied) AS tenants_replied,
               count(DISTINCT workspace_id) FILTER (WHERE won) AS tenants_won
        FROM (
            -- Opportunities: a submission requested-or-later is a send that
            -- left the building. `dismissed` stays out — it erases send
            -- evidence at the status column, so counting it would let a
            -- dismissed-after-send thread vanish while pretending the
            -- shape never happened.
            SELECT lower(btrim(opp.contact_email)) AS email_key, opp.workspace_id,
                   opp.status IN ('replied','won','lost') AS replied,
                   opp.status = 'won' AS won
            FROM viryaos_team_opportunities AS opp
            WHERE opp.contact_email IS NOT NULL
              AND opp.status IN ('submission_requested','submitted','replied','won','lost')
              AND lower(btrim(opp.contact_email)) IN (SELECT lower(btrim(k)) FROM unnest($1) AS k)
            UNION ALL
            -- Outreach targets: mailed is `last_outreach_at`; a reply is an
            -- inbound interaction row, not the denormalized disposition —
            -- a suppression stamp writes that column without a reply.
            SELECT lower(btrim(target.contact_email)), target.workspace_id,
                   EXISTS (
                       SELECT 1 FROM viryaos_outreach_interactions i
                       WHERE i.workspace_id = target.workspace_id
                         AND i.target_id = target.id
                         AND i.direction = 'inbound'
                   ),
                   EXISTS (
                       SELECT 1 FROM viryaos_outreach_interactions i
                       WHERE i.workspace_id = target.workspace_id
                         AND i.target_id = target.id
                         AND i.direction = 'inbound'
                         AND i.disposition = 'positive'
                   )
            FROM viryaos_outreach_targets AS target
            WHERE target.last_outreach_at IS NOT NULL
              AND lower(btrim(target.contact_email)) IN (SELECT lower(btrim(k)) FROM unnest($1) AS k)
            UNION ALL
            -- Booking targets: mailed is `last_outreach_at`; replies live in
            -- the interaction ledger — the target row has no reply columns.
            SELECT lower(btrim(booking.contact_email)), booking.workspace_id,
                   EXISTS (
                       SELECT 1 FROM viryaos_booking_interactions i
                       WHERE i.workspace_id = booking.workspace_id
                         AND i.target_id = booking.id
                         AND i.direction = 'inbound'
                   ),
                   EXISTS (
                       SELECT 1 FROM viryaos_booking_interactions i
                       WHERE i.workspace_id = booking.workspace_id
                         AND i.target_id = booking.id
                         AND i.direction = 'inbound'
                         AND i.disposition IN ('positive','booked')
                   )
            FROM viryaos_booking_targets AS booking
            WHERE booking.last_outreach_at IS NOT NULL
              AND lower(btrim(booking.contact_email)) IN (SELECT lower(btrim(k)) FROM unnest($1) AS k)
            UNION ALL
            -- Booking agents: the approach wrote an outbound interaction;
            -- replies are inbound rows of the same ledger.
            SELECT lower(btrim(agent.contact_email)), agent.workspace_id,
                   EXISTS (
                       SELECT 1 FROM viryaos_booking_agent_interactions i
                       WHERE i.workspace_id = agent.workspace_id
                         AND i.agent_id = agent.id
                         AND i.direction = 'inbound'
                   ),
                   EXISTS (
                       SELECT 1 FROM viryaos_booking_agent_interactions i
                       WHERE i.workspace_id = agent.workspace_id
                         AND i.agent_id = agent.id
                         AND i.direction = 'inbound'
                         AND i.disposition IN ('positive','signed')
                   )
            FROM viryaos_booking_agents AS agent
            WHERE EXISTS (
                SELECT 1 FROM viryaos_booking_agent_interactions o
                WHERE o.workspace_id = agent.workspace_id
                  AND o.agent_id = agent.id
                  AND o.direction = 'outbound'
            )
              AND lower(btrim(agent.contact_email)) IN (SELECT lower(btrim(k)) FROM unnest($1) AS k)
            UNION ALL
            -- Beacon threads: venue and promoter beacons are exactly the
            -- counterparty population; the campaign's reply disposition is
            -- a reply ledger column, written only on inbound.
            SELECT lower(btrim(beacon.contact_email)), campaign.workspace_id,
                   campaign.last_reply_disposition <> 'none',
                   campaign.last_reply_disposition IN ('interested','partner')
            FROM viryaos_beacon_campaigns AS campaign
            JOIN viryaos_beacons AS beacon
              ON beacon.workspace_id = campaign.workspace_id
             AND beacon.id = campaign.beacon_id
            WHERE campaign.last_outreach_at IS NOT NULL
              AND beacon.contact_email IS NOT NULL
              AND lower(btrim(beacon.contact_email)) IN (SELECT lower(btrim(k)) FROM unnest($1) AS k)
            UNION ALL
            -- The send ledger: every reserved outbound window. Touches, not
            -- the governor — a do-not-contact reply writes a governor row
            -- with a fabricated last_outbound_at, and only a real send
            -- writes a touch.
            SELECT lower(btrim(touch.normalized_contact)), touch.workspace_id,
                   false, false
            FROM viryaos_contact_touches AS touch
            WHERE lower(btrim(touch.normalized_contact)) IN (SELECT lower(btrim(k)) FROM unnest($1) AS k)
        ) attempts
        GROUP BY email_key
        "#,
    )
    .bind(email_keys)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(email_key, tenants_contacted, tenants_replied, tenants_won)| {
                (
                    email_key,
                    CounterpartyPrior {
                        tenants_contacted,
                        tenants_replied,
                        tenants_won,
                    },
                )
            },
        )
        .collect())
}

/// The play record for each venue in `venue_ids`, across every workspace —
/// the marks the registry trigger writes when a published or completed
/// event names the room. "Cold" for this tenant is not cold for the
/// registry: `tenants_played` is the comparables claim P.2 promised.
pub async fn venue_priors(
    pool: &PgPool,
    venue_ids: &[Uuid],
) -> Result<HashMap<Uuid, VenuePrior>, sqlx::Error> {
    if venue_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, i64, i64)>(
        r#"
        SELECT venue_id,
               count(DISTINCT workspace_id) AS tenants_played,
               count(*) AS shows
        FROM place_venue_marks
        WHERE venue_id = ANY($1)
        GROUP BY venue_id
        "#,
    )
    .bind(venue_ids)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(venue_id, tenants_played, shows)| {
            (
                venue_id,
                VenuePrior {
                    tenants_played,
                    shows,
                },
            )
        })
        .collect())
}
