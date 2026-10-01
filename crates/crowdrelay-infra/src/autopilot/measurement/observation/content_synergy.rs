//! Content-synergy observation arms.
//!
//! Split out of `observation.rs` so both stay inside the source-size
//! ratchet. These kinds measure what content *did* once it existed: the
//! clicks on the tracked link a social post carried, and whether a produced
//! artifact ever reached an audience as a post.
//!
//! # Clock and credit
//!
//! The seven-day window opens at each post's own `posted_at`, never at
//! `measurement.action_finished_at`: a draft that waited three days for a
//! person to publish it still gets its full seven days of observation, and
//! the claim-side deferral in `fan_windows` holds the measurement until that
//! window has actually closed. Conversion credit is the canonical ledger's —
//! `fan_provenance_events` rows written by `record_conversion` with
//! `attribution_method = 'last_tracked_click'` — so a person who clicked post
//! A and then post B before signing up is credited exactly once, to B, here
//! and in every other reader. Earlier clicks remain visible as journey
//! evidence; they are never a second conversion.

use super::*;

/// The action's tracked links with the real publication times of the posts
/// carrying them. `posted_at` is the "was live" fact in all four ledgers —
/// a draft, a failed send or a row awaiting a manual post has none, so an
/// unpublished link cannot open a window. The link resolves differently per
/// post table: social, Telegram and Discord posts carry `smart_link_id`
/// outright; a community post stores only the `/l/{slug}` path in
/// `smart_link`, resolved back to the `smart_links` row that serves it.
pub(in crate::autopilot::measurement) const POSTED_LINKS: &str = r#"
    SELECT post.smart_link_id AS link_id, post.posted_at
    FROM social_posts AS post
    WHERE post.workspace_id = $1 AND post.action_id = $2
      AND post.smart_link_id IS NOT NULL AND post.posted_at IS NOT NULL
    UNION ALL
    SELECT post.smart_link_id, post.posted_at
    FROM telegram_posts AS post
    WHERE post.workspace_id = $1 AND post.action_id = $2
      AND post.smart_link_id IS NOT NULL AND post.posted_at IS NOT NULL
    UNION ALL
    SELECT post.smart_link_id, post.posted_at
    FROM discord_posts AS post
    WHERE post.workspace_id = $1 AND post.action_id = $2
      AND post.smart_link_id IS NOT NULL AND post.posted_at IS NOT NULL
    UNION ALL
    SELECT link.id, post.posted_at
    FROM community_posts AS post
    JOIN smart_links AS link
      ON link.workspace_id = post.workspace_id
     AND link.slug = substring(post.smart_link from 4)
    WHERE post.workspace_id = $1 AND post.action_id = $2
      AND post.smart_link LIKE '/l/%' AND post.posted_at IS NOT NULL
"#;

/// Clicks on the link this post carried — joined through the post's own
/// tracked link, so the count is this action's traffic and not the
/// workspace's. A published post with no tracked link is unmeasurable rather
/// than a zero: the draft named a destination no redirect was minted for,
/// and "the link was never instrumented" is not "nobody clicked".
///
/// Each post's clicks are counted inside its own seven days from its own
/// `posted_at`; a click that landed before the post went live is impossible,
/// and one landing after its window belongs to a different measurement.
pub(super) async fn content_link_clicks(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    let (tracked, clicks) = sqlx::query_as::<_, (i64, f64)>(&CONTENT_LINK_CLICKS_SQL)
        .bind(workspace_id.into_uuid())
        .bind(measurement.action_id.into_uuid())
        .fetch_one(pool)
        .await
        .map_err(map_sqlx)?;
    if tracked == 0 {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NO_TRACKED_LINK,
        ));
    }
    Ok(clicks)
}

/// The credited post's own publication time: `source_target` on a
/// `last_tracked_click` conversion carries the clicked link's slug, so the
/// join back through `smart_links` recovers exactly the post the ledger
/// credited. When the link row is gone the window falls back to the action's
/// first live post — the credited fan is still counted rather than silently
/// dropped because a link row was retired.
// A FROM-clause fragment, not a statement — it joins the `posted_links` CTE
// its composed callers define, so it stays a plain string.
const CONVERSION_WINDOW: &str = "
    LEFT JOIN smart_links AS credited_link
      ON credited_link.workspace_id = conversion.workspace_id
     AND credited_link.slug = conversion.source_target
    LEFT JOIN posted_links AS link ON link.link_id = credited_link.id
    CROSS JOIN LATERAL (
        SELECT COALESCE(
            link.posted_at,
            (SELECT MIN(posted_at) FROM posted_links)
        ) AS opened_at
    ) AS credit_window
";

fn with_posted_links(sql: &str) -> String {
    sql.replace("/* posted_links */", POSTED_LINKS)
        .replace("/* conversion_window */", CONVERSION_WINDOW)
}

/// [`content_link_clicks`]' query — per-post windows from `posted_at`, so a
/// late-published draft still earns its whole seven days of observation.
static CONTENT_LINK_CLICKS_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    with_posted_links(
        r#"
        WITH posted_links AS (
            /* posted_links */
        )
        SELECT (SELECT COUNT(*) FROM posted_links)::bigint AS tracked_links,
               COUNT(click.*)::double precision AS clicks
        FROM posted_links AS link
        LEFT JOIN click_events AS click
          ON click.workspace_id = $1
         AND click.smart_link_id = link.link_id
         AND click.occurred_at >= link.posted_at
         AND click.occurred_at < link.posted_at + INTERVAL '7 days'
        "#,
    )
});

/// [`content_fan_acquisitions`]' query — NULL when the action never had a
/// live tracked post, so the caller can answer `no_tracked_link` instead of
/// mistaking an unmeasured publication for a measured zero.
static CONTENT_FAN_ACQUIRED_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    with_posted_links(
        r#"
        WITH posted_links AS (
            /* posted_links */
        ), acquired AS (
            SELECT DISTINCT conversion.fan_id
            FROM fan_provenance_events AS conversion
            JOIN fans AS fan
              ON fan.workspace_id = conversion.workspace_id
             AND fan.id = conversion.fan_id
             AND fan.status <> 'suppressed'
             AND fan.deleted_at IS NULL
            /* conversion_window */
            WHERE conversion.workspace_id = $1
              AND conversion.event_kind = 'conversion'
              AND conversion.attribution_method = 'last_tracked_click'
              AND conversion.action_id = $2
              AND conversion.occurred_at >= credit_window.opened_at
              AND conversion.occurred_at < credit_window.opened_at + INTERVAL '7 days'
        )
        SELECT CASE WHEN EXISTS (SELECT 1 FROM posted_links)
                    THEN (SELECT COUNT(*) FROM acquired)::double precision
               END
        "#,
    )
});

/// [`fan_ladder`]' query — the five funnel stages as one read sharing the
/// acquisition arm's exact window and credit rules.
static FAN_LADDER_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    with_posted_links(
        r#"
        WITH posted_links AS (
            /* posted_links */
        ), acquired AS (
            SELECT conversion.fan_id, MIN(conversion.occurred_at) AS converted_at
            FROM fan_provenance_events AS conversion
            JOIN fans AS fan
              ON fan.workspace_id = conversion.workspace_id
             AND fan.id = conversion.fan_id
             AND fan.status <> 'suppressed'
             AND fan.deleted_at IS NULL
            /* conversion_window */
            WHERE conversion.workspace_id = $1
              AND conversion.event_kind = 'conversion'
              AND conversion.attribution_method = 'last_tracked_click'
              AND conversion.action_id = $2
              AND conversion.occurred_at >= credit_window.opened_at
              AND conversion.occurred_at < credit_window.opened_at + INTERVAL '7 days'
            GROUP BY conversion.fan_id
        )
        SELECT
            (SELECT COUNT(DISTINCT click.anonymous_visitor_id)
               FROM posted_links AS link
               JOIN click_events AS click
                 ON click.workspace_id = $1
                AND click.smart_link_id = link.link_id
                AND click.occurred_at >= link.posted_at
                AND click.occurred_at < link.posted_at + INTERVAL '7 days'
               WHERE click.anonymous_visitor_id IS NOT NULL) AS unique_visitors,
            (SELECT COUNT(DISTINCT fan_id) FROM acquired) AS signups,
            (SELECT COUNT(DISTINCT acquired.fan_id)
               FROM acquired
               JOIN fans AS fan
                 ON fan.workspace_id = $1 AND fan.id = acquired.fan_id
              WHERE fan.status = 'active') AS confirmed,
            (SELECT COUNT(DISTINCT acquired.fan_id) FROM acquired JOIN fans fan ON fan.workspace_id=$1 AND fan.id=acquired.fan_id
              WHERE fan.status='active' AND fan.deleted_at IS NULL
                AND COALESCE((SELECT granted FROM fan_consents WHERE workspace_id=$1 AND fan_id=fan.id AND purpose='marketing'
                     AND recorded_at <= $3 ORDER BY recorded_at DESC,id DESC LIMIT 1),false)
                AND fan_has_engagement_between($1,fan.id,fan.normalized_email,acquired.converted_at+INTERVAL '1 microsecond',
                    LEAST(acquired.converted_at+INTERVAL '7 days',$3+INTERVAL '1 microsecond'))) AS activated,
            CASE WHEN $3 >= (SELECT MAX(posted_at) FROM posted_links)+INTERVAL '37 days'
              THEN (SELECT COUNT(DISTINCT acquired.fan_id) FROM acquired JOIN fans fan ON fan.workspace_id=$1 AND fan.id=acquired.fan_id
                WHERE fan.deleted_at IS NULL AND fan_is_meaningfully_retained($1,fan.id,acquired.converted_at,$3)
                  AND fan_has_engagement_between($1,fan.id,fan.normalized_email,
                      GREATEST(acquired.converted_at+INTERVAL '30 days',$3-INTERVAL '30 days'),$3+INTERVAL '1 microsecond'))
            END AS returned
        "#,
    )
});

/// Fans this action's tracked publication acquired — counted from the same
/// `fan_provenance_events` conversion rows the canonical ledger writes at
/// signup (`attribution_method = 'last_tracked_click'`).
///
/// Why not click→signup joins: a person who clicked post A and then post B
/// before signing up converted once, and the ledger credits the *last*
/// tracked click — post B. Any-click joins credit both posts, and
/// `COUNT(DISTINCT fan_id)` inside one post's query cannot deduplicate
/// across posts. Reading the ledger's own assignment keeps every reader's
/// number identical to the ledger's, replay-safe, and single-owned.
///
/// The window is the credited post's own `posted_at` + 7 days. Suppressed
/// and deleted fans stop counting — consent withdrawal is a fact about the
/// person, not a retroactive edit of the ledger row.
pub(super) async fn content_fan_acquisitions(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    sqlx::query_scalar::<_, Option<f64>>(&CONTENT_FAN_ACQUIRED_SQL)
        .bind(workspace_id.into_uuid())
        .bind(measurement.action_id.into_uuid())
        .fetch_one(pool)
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NO_TRACKED_LINK,
        ))
}

/// The publication's fan funnel as separate facts — never a single "fans"
/// number smuggling confirmation, activation and return into one. Each stage
/// names only what it can honestly see: `unique_visitors` are distinct
/// clickers, `signups` are canonical conversions, `confirmed` reached
/// `status = 'active'` (double opt-in), `activated` produced deliberate first-party engagement after conversion provenance row within seven days of signing up.
///
/// `returned` is NULL until the latest credited post's full 7-day acquisition plus 30-day durability
/// horizon has passed — an unobserved stage is an unknown, not a zero. The
/// number beside the learned scalar is reporting only; the selector keeps
/// learning from `signups` alone.
#[derive(Debug, Clone, serde::Serialize)]
pub(in crate::autopilot::measurement) struct FanLadder {
    pub unique_visitors: i64,
    pub signups: i64,
    pub confirmed: i64,
    pub activated: i64,
    pub returned: Option<i64>,
}

/// Reads the ladder for the outcome metadata of `content_fan_acquisition_7d`.
/// Called only after the acquisition arm succeeded, so a tracked post is
/// known to exist; the ladder shares its exact window and credit rules.
pub(in crate::autopilot::measurement) async fn fan_ladder(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
    now: OffsetDateTime,
) -> Result<FanLadder, RepositoryError> {
    sqlx::query_as::<_, (i64, i64, i64, i64, Option<i64>)>(&FAN_LADDER_SQL)
        .bind(workspace_id.into_uuid())
        .bind(measurement.action_id.into_uuid())
        .bind(now)
        .fetch_one(pool)
        .await
        .map(
            |(unique_visitors, signups, confirmed, activated, returned)| FanLadder {
                unique_visitors,
                signups,
                confirmed,
                activated,
                returned,
            },
        )
        .map_err(map_sqlx)
}

/// Posts filed against the artifact's content source in the week after
/// production was confirmed. The receipt already proved the artifact exists,
/// so the count answers only for whether it reached an audience — zero is
/// the produced-and-never-posted verdict, not a missing instrument. The join
/// runs through the posting action's `source_id` — `payload.source_id` where
/// the payload declares it outright, `payload.draft.source_id` where the
/// draft carries the source — across all four post tables, because a
/// produced clip goes up wherever the channel action references it.
pub(super) async fn artifact_outcome(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COUNT(*)::double precision
        FROM (
            SELECT post.posted_at
            FROM community_posts AS post
            JOIN autopilot_actions AS act
              ON act.workspace_id = post.workspace_id
             AND act.id = post.action_id
            WHERE post.workspace_id = $1
              AND post.status = 'posted'
              AND (lower(act.payload->>'source_id') = $2::text
                   OR lower(act.payload->'draft'->>'source_id') = $2::text)
            UNION ALL
            SELECT post.posted_at
            FROM social_posts AS post
            JOIN autopilot_actions AS act
              ON act.workspace_id = post.workspace_id
             AND act.id = post.action_id
            WHERE post.workspace_id = $1
              AND post.status = 'posted'
              AND (lower(act.payload->>'source_id') = $2::text
                   OR lower(act.payload->'draft'->>'source_id') = $2::text)
            UNION ALL
            SELECT post.posted_at
            FROM telegram_posts AS post
            JOIN autopilot_actions AS act
              ON act.workspace_id = post.workspace_id
             AND act.id = post.action_id
            WHERE post.workspace_id = $1
              AND post.status = 'posted'
              AND (lower(act.payload->>'source_id') = $2::text
                   OR lower(act.payload->'draft'->>'source_id') = $2::text)
            UNION ALL
            SELECT post.posted_at
            FROM discord_posts AS post
            JOIN autopilot_actions AS act
              ON act.workspace_id = post.workspace_id
             AND act.id = post.action_id
            WHERE post.workspace_id = $1
              AND post.status = 'posted'
              AND (lower(act.payload->>'source_id') = $2::text
                   OR lower(act.payload->'draft'->>'source_id') = $2::text)
        ) AS posts
        WHERE posts.posted_at >= $3
          AND posts.posted_at < $3 + INTERVAL '7 days'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.subject_id)
    .bind(measurement.action_finished_at)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}
