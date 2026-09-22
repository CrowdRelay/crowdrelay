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
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        r#"
        SELECT channel,
               COUNT(DISTINCT fan_id)
                   FILTER (WHERE event_kind = 'conversion')::bigint
                   AS conversions,
               COUNT(DISTINCT COALESCE(fan_id::text, anonymous_visitor_id::text))
                   FILTER (WHERE event_kind = 'interaction')::bigint
                   AS unique_clickers
        FROM fan_provenance_events
        WHERE workspace_id = $1
          AND event_kind IN ('conversion', 'interaction')
          AND occurred_at >= now() - interval '30 days'
        GROUP BY channel
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(rows
        .into_iter()
        .map(|(channel, conversions, clickers)| ChannelYield {
            channel,
            conversions_30d: u32::try_from(conversions.max(0)).unwrap_or(u32::MAX),
            unique_clickers_30d: u32::try_from(clickers.max(0)).unwrap_or(u32::MAX),
        })
        .collect())
}
