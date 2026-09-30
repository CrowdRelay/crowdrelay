//! The pooled half of roster-level source ROI (5.4, N.6): one organisation's
//! acquisition channels, counted across every act at once.
//!
//! The shape is deliberately the per-act read's shape. `acquisition_channels`
//! walks a fan back to the newest click at or before their signup and reads the
//! channel off that link; this walks the same path for every act in the
//! organisation and groups the result by channel instead of by act. Two
//! different walks would be two different definitions of "where this person came
//! from", and the roster number would then disagree with the act number that
//! composes it, with nothing to say which was right.
//!
//! Two deliberate differences from the per-act read:
//!
//! * **Creative is dropped from the grouping key.** A creative is one act's
//!   poster or one act's wording; pooling `virya-poster-v2` with a labelmate's
//!   own artwork answers nothing. Source and community travel between acts,
//!   which is the only thing a roster can act on.
//! * **The act count is measured, not summed.** `count(DISTINCT workspace_id)`
//!   per channel, in SQL. Adding up per-row act counts would count an act twice
//!   as soon as it ran two creatives, and the "more than one act contributed"
//!   rule the domain enforces would then pass on a single act's traffic.
//!
//! Churn stays in the denominator. Source ROI asks what share of acquired people
//! were still real fans after a full 30-day window, so an unsubscribe cannot
//! erase the acquisition that preceded it. Only arrivals between 30 and 90 days
//! old are eligible: newer people have not had time to prove retention, and
//! older cohorts belong to a different operating period.

use crowdrelay_domain::acquisition_channel::{
    AttributionEvidence, ChannelAttribution, ChannelIdentity, attribute_channel,
};
use crowdrelay_domain::roster_source_roi::{ChannelSample, PooledCounts};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

/// A roster wider than this is not read in one pass. Same bound the roster
/// plan uses, for the same reason: one query holding a connection open across
/// an unbounded number of acts is how a read becomes an outage.
const MAX_ACTS: i64 = 60;

/// At most this many channel groups come back. A roster with more distinct
/// source/community pairs than this has a link-labelling problem, not a
/// ranking problem, and the biggest groups are the ones a ranking is about.
const MAX_CHANNELS: i64 = 200;

#[derive(Debug, FromRow)]
struct PooledChannelRow {
    had_visitor: bool,
    had_click: bool,
    channel_source: Option<String>,
    channel_community: Option<String>,
    acts: i64,
    signups: i64,
    stayed_30d: i64,
}

/// Every act in the organisation's channels, pooled.
///
/// Returns the counts only. The ranking, the evidence floor and the finding are
/// the domain's, so that the rule about what a pooled number may claim lives
/// somewhere a test can reach without a database.
///
/// # Errors
///
/// Propagates the database error.
pub async fn pooled_channel_counts(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<PooledCounts, sqlx::Error> {
    let workspaces = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT workspace.id
        FROM workspaces AS workspace
        WHERE workspace.organization_id = $1
        ORDER BY workspace.name, workspace.id
        LIMIT $2
        "#,
    )
    .bind(organization_id)
    .bind(MAX_ACTS)
    .fetch_all(pool)
    .await?;

    if workspaces.is_empty() {
        return Ok(PooledCounts::default());
    }

    let rows = sqlx::query_as::<_, PooledChannelRow>(
        r#"
        WITH arrival AS (
            SELECT
                fan.id AS fan_id,
                fan.workspace_id,
                fan.normalized_email,
                acquisition.anonymous_visitor_id,
                COALESCE(acquisition.occurred_at, fan.created_at) AS signed_up_at,
                fan.status = 'active' AS stayed_active,
                CASE WHEN fan.status = 'active' THEN fan_last_meaningful_action(
                    fan.workspace_id, fan.id, fan.normalized_email
                ) END AS last_action_at,
                EXISTS (
                    SELECT 1 FROM fan_consents AS consent
                    WHERE consent.workspace_id = fan.workspace_id
                      AND consent.fan_id = fan.id
                      AND consent.purpose = 'marketing'
                      AND consent.granted
                      AND consent.recorded_at = (
                          SELECT max(latest.recorded_at) FROM fan_consents AS latest
                          WHERE latest.workspace_id = fan.workspace_id
                            AND latest.fan_id = fan.id
                            AND latest.purpose = 'marketing'
                      )
                ) AS consented
            FROM fans AS fan
            -- The *first* acquisition event is the acquisition, exactly as the
            -- per-act read has it: a later one is a return visit, and crediting
            -- the channel somebody came back through robs the one that found
            -- them.
            LEFT JOIN LATERAL (
                SELECT event.anonymous_visitor_id, event.occurred_at
                FROM fan_acquisition_events AS event
                WHERE event.workspace_id = fan.workspace_id
                  AND event.fan_id = fan.id
                ORDER BY event.occurred_at ASC, event.id ASC
                LIMIT 1
            ) AS acquisition ON true
            WHERE fan.workspace_id = ANY($1)
              AND fan.status IN ('active', 'unsubscribed', 'suppressed')
              AND COALESCE(acquisition.occurred_at, fan.created_at)
                    BETWEEN $2 - INTERVAL '90 days' AND $2 - INTERVAL '30 days'
        ), attributed AS (
            SELECT
                arrival.workspace_id,
                arrival.anonymous_visitor_id IS NOT NULL AS had_visitor,
                click.smart_link_id IS NOT NULL AS had_click,
                link.channel_source,
                link.channel_community,
                (
                    arrival.stayed_active
                    AND arrival.consented
                    AND arrival.last_action_at IS NOT NULL
                    AND arrival.last_action_at >= GREATEST(
                        $2 - INTERVAL '30 days',
                        arrival.signed_up_at + INTERVAL '30 days'
                    )
                    AND arrival.last_action_at <= $2
                ) AS stayed
            FROM arrival
            -- Each act's own clicks and its own links. Scoping these to the
            -- fan's workspace rather than to the organisation is what keeps a
            -- labelmate's visitor from ever attributing this act's signup.
            LEFT JOIN LATERAL (
                SELECT click.smart_link_id
                FROM click_events AS click
                WHERE click.workspace_id = arrival.workspace_id
                  AND arrival.anonymous_visitor_id IS NOT NULL
                  AND click.anonymous_visitor_id = arrival.anonymous_visitor_id
                  AND (arrival.signed_up_at IS NULL OR click.occurred_at <= arrival.signed_up_at)
                ORDER BY click.occurred_at DESC, click.id DESC
                LIMIT 1
            ) AS click ON true
            LEFT JOIN smart_links AS link
              ON link.workspace_id = arrival.workspace_id
             AND link.id = click.smart_link_id
        )
        SELECT
            had_visitor,
            had_click,
            channel_source,
            channel_community,
            count(DISTINCT workspace_id)::bigint AS acts,
            count(*)::bigint AS signups,
            count(*) FILTER (WHERE stayed)::bigint AS stayed_30d
        FROM attributed
        GROUP BY had_visitor, had_click, channel_source, channel_community
        ORDER BY signups DESC, channel_source
        LIMIT $3
        "#,
    )
    .bind(&workspaces)
    .bind(now)
    .bind(MAX_CHANNELS)
    .fetch_all(pool)
    .await?;

    let acts_with_fans = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(DISTINCT fan.workspace_id)::bigint
        FROM fans AS fan
        WHERE fan.workspace_id = ANY($1)
          AND fan.status = 'active'
        "#,
    )
    .bind(&workspaces)
    .fetch_one(pool)
    .await?;

    let mut samples: Vec<ChannelSample> = Vec::new();
    let mut unattributed_signups: u32 = 0;

    for row in rows {
        let signups = clamp(row.signups);
        // Classified by the domain, not by this query's WHERE clause. The rule
        // that a signup with no click is unattributed rather than "direct"
        // lives in exactly one place, and a second copy here is a second place
        // for it to drift.
        let attribution = attribute_channel(&AttributionEvidence {
            had_visitor: row.had_visitor,
            had_click_before_signup: row.had_click,
            identity: row.channel_source.map(|source| ChannelIdentity {
                source,
                community: row.channel_community,
                // Deliberately dropped from the key: see the module header.
                creative: None,
            }),
        });
        match attribution {
            ChannelAttribution::Unattributed { .. } => {
                unattributed_signups = unattributed_signups.saturating_add(signups);
            }
            ChannelAttribution::Attributed(identity) => samples.push(ChannelSample {
                source: identity.source,
                community: identity.community,
                acts: u16::try_from(row.acts.max(0)).unwrap_or(u16::MAX),
                signups,
                stayed_30d: clamp(row.stayed_30d),
            }),
        }
    }

    Ok(PooledCounts {
        samples,
        acts_with_fans: u16::try_from(acts_with_fans.max(0)).unwrap_or(u16::MAX),
        unattributed_signups,
    })
}

/// A count from Postgres is an `i64` and cannot be negative here; saturating
/// rather than erroring keeps one impossible row from failing the whole read.
fn clamp(count: i64) -> u32 {
    u32::try_from(count.max(0)).unwrap_or(u32::MAX)
}
