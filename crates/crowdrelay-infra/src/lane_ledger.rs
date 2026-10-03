//! The per-lane outcome counts behind the lane ledger.
//!
//! Four post tables share one status vocabulary (`pending`, `posting`,
//! `posted`, `failed`, `rate_limited`, `awaiting_manual_post`; community posts
//! also have `cancelled`). A lane is one platform on one of them. Every
//! statement names `workspace_id`; the window is on the row's own `created_at`
//! (when the band asked), so a delivered-but-old post does not make a lane that
//! has been stuck all week look alive.

use crowdrelay_domain::lane_ledger::{LaneCounts, Outcome};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// One lane, its counts, and how long its oldest unfinished request has waited.
#[derive(Debug)]
pub struct LaneRow {
    /// `reddit`, `forum`, `instagram`, `facebook`, `telegram`, `discord_channel`, …
    pub lane: String,
    pub counts: LaneCounts,
    /// Hours since the oldest request that is neither delivered, failed nor
    /// withdrawn was created. `None` when nothing is unfinished.
    pub oldest_unfinished_hours: Option<i64>,
    /// A status the vocabulary does not know, if any row carried one: counted
    /// nowhere, reported here so it cannot hide.
    pub unknown_statuses: u32,
}

#[derive(Debug, FromRow)]
struct Row {
    lane: String,
    status: String,
    n: i64,
    oldest_unfinished_hours: Option<i64>,
}

/// # Errors
///
/// Propagates the database error.
pub async fn lane_rows(
    pool: &PgPool,
    workspace_id: Uuid,
    days: i32,
) -> Result<Vec<LaneRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, Row>(
        r#"
        SELECT lane, status, count(*)::bigint AS n,
               max(CASE WHEN status IN ('pending','posting','rate_limited','awaiting_manual_post')
                        THEN FLOOR(EXTRACT(EPOCH FROM (now() - created_at)) / 3600.0)::bigint
                   END) AS oldest_unfinished_hours
        FROM (
            SELECT platform AS lane, status, created_at
              FROM community_posts
             WHERE workspace_id = $1 AND created_at >= now() - make_interval(days => $2)
            UNION ALL
            SELECT platform AS lane, status, created_at
              FROM social_posts
             WHERE workspace_id = $1 AND created_at >= now() - make_interval(days => $2)
            UNION ALL
            SELECT 'telegram' AS lane, status, created_at
              FROM telegram_posts
             WHERE workspace_id = $1 AND created_at >= now() - make_interval(days => $2)
            UNION ALL
            SELECT 'discord_channel' AS lane, status, created_at
              FROM discord_posts
             WHERE workspace_id = $1 AND created_at >= now() - make_interval(days => $2)
        ) posts
        GROUP BY lane, status
        ORDER BY lane, status
        "#,
    )
    .bind(workspace_id)
    .bind(days)
    .fetch_all(pool)
    .await?;
    let mut lanes: std::collections::BTreeMap<String, LaneRow> = std::collections::BTreeMap::new();
    for row in rows {
        let lane = lanes.entry(row.lane.clone()).or_insert_with(|| LaneRow {
            lane: row.lane.clone(),
            counts: LaneCounts::default(),
            oldest_unfinished_hours: None,
            unknown_statuses: 0,
        });
        let n = u32::try_from(row.n.max(0)).unwrap_or(u32::MAX);
        match Outcome::from_status(&row.status) {
            Some(outcome) => lane.counts.add(outcome, n),
            None => lane.unknown_statuses = lane.unknown_statuses.saturating_add(n),
        }
        lane.oldest_unfinished_hours = lane
            .oldest_unfinished_hours
            .max(row.oldest_unfinished_hours);
    }
    let lanes: Vec<LaneRow> = lanes.into_values().collect();
    Ok(lanes)
}

/// The tenant's own switches for publishing without asking, as stored. `None`
/// means the row is absent (the domain default applies); it is not "off".
#[derive(Debug, Default, FromRow)]
pub struct AutopostSettings {
    pub social_auto_post: Option<String>,
    pub social_autopost_platforms: Option<String>,
}

/// # Errors
///
/// Propagates the database error.
pub async fn autopost_settings(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<AutopostSettings, sqlx::Error> {
    sqlx::query_as::<_, AutopostSettings>(
        "SELECT
           (SELECT value FROM tenant_settings
             WHERE workspace_id = $1 AND key = 'social_auto_post') AS social_auto_post,
           (SELECT value FROM tenant_settings
             WHERE workspace_id = $1 AND key = 'social_autopost_platforms') AS social_autopost_platforms",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await
}
