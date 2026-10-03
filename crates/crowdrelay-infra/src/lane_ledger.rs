//! The per-lane outcome counts behind the lane ledger.
//!
//! Four post tables share one status vocabulary (`pending`, `posting`,
//! `posted`, `failed`, `rate_limited`, `awaiting_manual_post`; community posts
//! also have `cancelled`). A lane is one platform on one of them. Every
//! statement names `workspace_id`; the window is on the row's own `created_at`
//! (when the band asked), so a delivered-but-old post does not make a lane that
//! has been stuck all week look alive.

use crowdrelay_domain::lane_ledger::{LaneCounts, LaneScope, Outcome};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// One lane, its counts, and how long its oldest unfinished request has waited.
#[derive(Debug)]
pub struct LaneRow {
    /// The authority surface that produced the row. A community Telegram and
    /// the band's owned Telegram are different lanes even though both use the
    /// same platform name.
    pub scope: LaneScope,
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
    scope: String,
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
        SELECT scope, lane, status, count(*)::bigint AS n,
               max(CASE WHEN status IN ('pending','posting','rate_limited','awaiting_manual_post')
                        THEN FLOOR(EXTRACT(EPOCH FROM (now() - created_at)) / 3600.0)::bigint
                   END) AS oldest_unfinished_hours
        FROM (
            SELECT 'community' AS scope, platform AS lane, status, created_at
              FROM community_posts
             WHERE workspace_id = $1 AND created_at >= now() - make_interval(days => $2)
            UNION ALL
            SELECT 'owned' AS scope, platform AS lane, status, created_at
              FROM social_posts
             WHERE workspace_id = $1 AND created_at >= now() - make_interval(days => $2)
            UNION ALL
            SELECT 'owned' AS scope, 'telegram' AS lane, status, created_at
              FROM telegram_posts
             WHERE workspace_id = $1 AND created_at >= now() - make_interval(days => $2)
            UNION ALL
            SELECT 'owned' AS scope, 'discord_channel' AS lane, status, created_at
              FROM discord_posts
             WHERE workspace_id = $1 AND created_at >= now() - make_interval(days => $2)
        ) posts
        GROUP BY scope, lane, status
        ORDER BY scope, lane, status
        "#,
    )
    .bind(workspace_id)
    .bind(days)
    .fetch_all(pool)
    .await?;
    let mut lanes: std::collections::BTreeMap<(LaneScope, String), LaneRow> =
        std::collections::BTreeMap::new();
    for row in rows {
        let scope = match row.scope.as_str() {
            "community" => LaneScope::Community,
            "owned" => LaneScope::Owned,
            _ => continue,
        };
        let lane = lanes
            .entry((scope, row.lane.clone()))
            .or_insert_with(|| LaneRow {
                scope,
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

/// The same ledger projected to the compact shape Autopilot routes on.
pub async fn lane_verdicts(
    pool: &PgPool,
    workspace_id: Uuid,
    days: i32,
) -> Result<Vec<(LaneScope, String, crowdrelay_domain::lane_ledger::Verdict)>, sqlx::Error> {
    Ok(lane_rows(pool, workspace_id, days)
        .await?
        .into_iter()
        .map(|row| {
            (
                row.scope,
                row.lane,
                crowdrelay_domain::lane_ledger::verdict(&row.counts),
            )
        })
        .collect())
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

/// The facts `crowdrelay_domain::day_zero::assess` reads, from first-party rows.
///
/// # Errors
///
/// Propagates the database error.
pub async fn day_zero_facts(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<DayZeroFacts, sqlx::Error> {
    let settings = crate::tenant_settings::TenantSettingsRepository::new(pool.clone());
    let brand = settings.brand_settings(workspace_id).await?;
    let explicit_join_copy = settings.join_ask_config(workspace_id).await?.is_some();
    let grounded_join_copy =
        crate::join_ask::load_grounded_starter_variant(pool, workspace_id).await?;
    let join_copy = explicit_join_copy || grounded_join_copy.is_some();
    let fresh_asset: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM content_sources
            WHERE workspace_id = $1 AND active
              AND source_kind IN ('video', 'release', 'event', 'social_post')
              AND expires_at > now()
              AND occurred_at > now() - interval '30 days'
        )
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;
    let social_publish_runtime: Option<bool> = sqlx::query_scalar(
        "SELECT enabled FROM growth_component_state
         WHERE workspace_id = $1 AND component = 'social_post_executor'",
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?;

    let (recorded_scopes, check_age_hours) = recorded_publish_scopes(pool, workspace_id).await?;
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        r#"
        SELECT platform, status, COALESCE(health, 'unverified')
        FROM fanbase_connections
        WHERE workspace_id = $1 AND platform IN ('facebook', 'instagram')
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;
    let connections = rows
        .into_iter()
        .map(|(platform, status, health)| {
            let publish = crowdrelay_domain::day_zero::publish_permission(
                &platform,
                recorded_scopes.as_deref(),
                check_age_hours,
            );
            crowdrelay_domain::day_zero::ConnectionFact {
                platform,
                connected: status == "connected",
                working: health == "working",
                publish,
            }
        })
        .collect();
    Ok(DayZeroFacts {
        site_root: brand.site_root().map(str::to_owned),
        join_copy,
        fresh_asset,
        social_publish_runtime,
        social_auto_post: brand.social_auto_post,
        autopost_platforms: brand.social_autopost_platforms.clone(),
        connections,
    })
}

/// Owned form of the readiness inputs, so the caller can hold it while the pure
/// decision borrows from it.
#[derive(Debug)]
pub struct DayZeroFacts {
    pub site_root: Option<String>,
    pub join_copy: bool,
    pub fresh_asset: bool,
    pub social_publish_runtime: Option<bool>,
    pub social_auto_post: bool,
    pub autopost_platforms: Vec<String>,
    pub connections: Vec<crowdrelay_domain::day_zero::ConnectionFact>,
}

impl DayZeroFacts {
    /// Public-social platforms that can genuinely publish without another
    /// person right now. Derived from the same readiness facts the control
    /// plane shows; no second interpretation of authority is allowed.
    #[must_use]
    pub fn executable_owned_social_platforms(&self) -> Vec<String> {
        let readiness = self.assess();
        if !readiness.ready {
            return Vec::new();
        }
        readiness
            .rails
            .into_iter()
            .filter(|rail| rail.state == crowdrelay_domain::day_zero::RailState::Executable)
            .map(|rail| rail.platform.to_owned())
            .collect()
    }

    /// Whether the narrow Facebook standing-authority handoff may be accepted.
    ///
    /// The deployment gate is deliberately part of this check: storing tenant
    /// authority while the worker cannot publish would recreate the false-ready
    /// state this Day-0 surface exists to eliminate.
    #[must_use]
    pub fn facebook_authority_grantable(&self) -> bool {
        self.site_root
            .as_deref()
            .is_some_and(|root| !root.trim().is_empty())
            && self.join_copy
            && self.fresh_asset
            && self.social_publish_runtime == Some(true)
            && self.connections.iter().any(|connection| {
                connection.platform.eq_ignore_ascii_case("facebook")
                    && connection.connected
                    && connection.working
            })
    }

    /// The decision over these facts.
    #[must_use]
    pub fn assess(&self) -> crowdrelay_domain::day_zero::Readiness {
        crowdrelay_domain::day_zero::assess(&crowdrelay_domain::day_zero::ReadinessFacts {
            site_root: self.site_root.as_deref(),
            join_copy: self.join_copy,
            fresh_asset: self.fresh_asset,
            social_publish_runtime: self.social_publish_runtime,
            social_auto_post: self.social_auto_post,
            autopost_platforms: &self.autopost_platforms,
            connections: &self.connections,
        })
    }
}

/// `tenant_settings` keys holding the last platform-confirmed scope check.
///
/// Stored beside the other tenant facts rather than on a connection row: the
/// Meta publish credential is one worker-held token that covers both the
/// Facebook Page and the Instagram account, so the answer belongs to the token,
/// not to either connection.
pub const KEY_META_PUBLISH_SCOPES: &str = "meta_publish_scopes";
pub const KEY_META_PUBLISH_CHECKED_AT: &str = "meta_publish_scopes_checked_at";

/// The last recorded scope check and how many hours old it is, or `(None, None)`
/// when nothing has been recorded.
async fn recorded_publish_scopes(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<(Option<Vec<String>>, Option<i64>), sqlx::Error> {
    let row: Option<(Option<String>, Option<i64>)> = sqlx::query_as(
        r#"
        SELECT
            (SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = $2),
            (SELECT (EXTRACT(EPOCH FROM now() - value::timestamptz) / 3600)::bigint
               FROM tenant_settings
              WHERE workspace_id = $1 AND key = $3
                AND value ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T')
        "#,
    )
    .bind(workspace_id)
    .bind(KEY_META_PUBLISH_SCOPES)
    .bind(KEY_META_PUBLISH_CHECKED_AT)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((Some(scopes), Some(age))) => (
            Some(scopes.split_whitespace().map(str::to_owned).collect()),
            Some(age),
        ),
        _ => (None, None),
    })
}

/// Records what the platform said about the publish token's scopes.
///
/// Called only with an answer the platform actually gave: a check that could
/// not run records nothing, so an old answer ages out instead of being
/// overwritten with a guess. An empty list is a real answer.
///
/// # Errors
///
/// Propagates the database error.
pub async fn record_publish_scopes(
    pool: &PgPool,
    workspace_id: Uuid,
    scopes: &[String],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for (key, value) in [
        (KEY_META_PUBLISH_SCOPES, scopes.join(" ")),
        (
            KEY_META_PUBLISH_CHECKED_AT,
            time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        ),
    ] {
        sqlx::query(
            "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(workspace_id)
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
