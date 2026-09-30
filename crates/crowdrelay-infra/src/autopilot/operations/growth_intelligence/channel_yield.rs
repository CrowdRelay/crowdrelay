//! The attributed half of "where growth comes from" — per-channel arrivals.
//!
//! `platform_growth` counts followers going up on a platform — correlational,
//! and blind to whether anyone arrived. This loader reads the provenance
//! ledger instead: fans a channel's tracked links converted, and the distinct
//! visitors who clicked them. It is the query that turns attribution from
//! measurement into control — a channel that delivered a person is the one
//! worth posting on next.
//!
//! Kept in its own module for the same reason `community_targets` is: the
//! loader is one query with one contract, and the world-model assembly above
//! stays readable when each evidence source lives behind a name.

use crowdrelay_brain::platform_yield::ChannelYield;
use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;

use super::{RepositoryError, map_sqlx};

/// Loads each channel's attributed arrivals over the last 30 days.
///
/// Conversions count `DISTINCT fan_id` — a fan can carry two attributions (a
/// tracked click and a referral) and must not read as two fans. Clickers
/// dedupe on `COALESCE(fan_id, anonymous_visitor_id)` so a visitor counted
/// anonymously at click time is not counted twice after signup links them —
/// the same person must read as one.
///
/// Channels with no rows are absent, not zero: an unmeasured channel ranks
/// with the other unmeasured templates rather than behind them.
pub(super) async fn load_channel_yield(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<ChannelYield>, RepositoryError> {
    // Durable fans: only conversions old enough to have had a full 30-day
    // retention window can earn the retention bonus. They must still be active,
    // still consented to marketing (latest record), and have a meaningful
    // action both after that 30-day anniversary and inside the current 30-day
    // window. A fan acquired yesterday can contribute a conversion today, but
    // cannot also be called durable before thirty days have actually elapsed.
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        r#"
        WITH evidence AS (
            SELECT channel, event_kind, fan_id, anonymous_visitor_id, occurred_at
            FROM fan_provenance_events
            WHERE workspace_id = $1
              AND event_kind IN ('conversion', 'interaction')
              AND occurred_at >= now() - interval '90 days'
        ), durable AS (
            SELECT evidence.channel, COUNT(DISTINCT fan.id)::bigint AS durable
            FROM evidence
            JOIN fans AS fan
              ON fan.workspace_id = $1
             AND fan.id = evidence.fan_id
             AND fan.status = 'active'
            WHERE evidence.event_kind = 'conversion'
              AND evidence.occurred_at <= now() - interval '30 days'
              AND EXISTS (
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
              )
              AND fan_last_meaningful_action(fan.workspace_id, fan.id, fan.normalized_email)
                  >= GREATEST(
                      now() - interval '30 days',
                      evidence.occurred_at + interval '30 days'
                  )
            GROUP BY evidence.channel
        )
        SELECT evidence.channel,
               COUNT(DISTINCT evidence.fan_id)
                   FILTER (WHERE evidence.event_kind = 'conversion'
                             AND evidence.occurred_at >= now() - interval '30 days')::bigint
                   AS conversions,
               COUNT(DISTINCT COALESCE(evidence.fan_id::text, evidence.anonymous_visitor_id::text))
                   FILTER (WHERE evidence.event_kind = 'interaction'
                             AND evidence.occurred_at >= now() - interval '30 days')::bigint
                   AS unique_clickers,
               COALESCE(MAX(durable.durable), 0)::bigint AS durable
        FROM evidence
        LEFT JOIN durable ON durable.channel = evidence.channel
        GROUP BY evidence.channel
        HAVING COUNT(*) FILTER (WHERE evidence.occurred_at >= now() - interval '30 days') > 0
            OR COALESCE(MAX(durable.durable), 0) > 0
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(rows
        .into_iter()
        .map(|(channel, conversions, clickers, durable)| ChannelYield {
            channel,
            conversions_30d: u32::try_from(conversions.max(0)).unwrap_or(u32::MAX),
            unique_clickers_30d: u32::try_from(clickers.max(0)).unwrap_or(u32::MAX),
            durable_90d: u32::try_from(durable.max(0)).unwrap_or(u32::MAX),
        })
        .collect())
}
