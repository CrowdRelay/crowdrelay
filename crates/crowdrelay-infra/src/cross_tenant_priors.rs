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
//! not an oversight — and the workspace-scope ratchet lists this file for
//! exactly that reason. Anything added here must keep the same shape:
//! aggregates over `workspace_id`, never per-tenant rows.

use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

/// One address's cross-tenant reply record — counts only.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct CounterpartyPrior {
    /// Distinct tenants whose records show a send to this address — an
    /// opportunity submitted, an outreach or booking target written, or a
    /// governor touch, whichever the tenant's ledger kept.
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
/// A send is: a submitted opportunity naming the address, an outreach or
/// booking target the tenant has mailed, or a governor touch — the last
/// catches the sends (latarnik invites, T+7 reports) the typed tables miss.
/// A reply is an opportunity that reached `replied`/`won`/`lost` or an
/// outreach target whose last disposition is a real reply. Nothing here
/// reads message content — the ledger is statuses and timestamps.
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
            SELECT lower(btrim(opp.contact_email)) AS email_key, opp.workspace_id,
                   opp.status IN ('replied','won','lost') AS replied,
                   opp.status = 'won' AS won
            FROM viryaos_team_opportunities AS opp
            WHERE opp.contact_email IS NOT NULL
              AND opp.status IN ('submitted','replied','won','lost')
            UNION ALL
            SELECT lower(btrim(target.contact_email)), target.workspace_id,
                   target.last_reply_at IS NOT NULL
                       AND target.last_reply_disposition <> 'none',
                   target.last_reply_disposition = 'positive'
            FROM viryaos_outreach_targets AS target
            WHERE target.last_outreach_at IS NOT NULL
            UNION ALL
            SELECT lower(btrim(booking.contact_email)), booking.workspace_id,
                   false, false
            FROM viryaos_booking_targets AS booking
            WHERE booking.last_outreach_at IS NOT NULL
            UNION ALL
            SELECT governor.normalized_contact, governor.workspace_id,
                   false, false
            FROM viryaos_contact_governor AS governor
        ) attempts
        WHERE email_key = ANY($1)
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
