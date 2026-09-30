//! Content-synergy observation arms.
//!
//! Split out of `observation.rs` so both stay inside the source-size
//! ratchet. These kinds measure what content *did* once it existed: the
//! clicks on the tracked link a social post carried, and whether a produced
//! artifact ever reached an audience as a post.

use super::*;

/// Clicks on the link this post carried — joined through the post's own
/// tracked link, so the count is this action's traffic and not the
/// workspace's. The link resolves differently per post table: social,
/// Telegram and Discord posts carry `smart_link_id` outright; a community
/// post stores only the `/l/{slug}` path in `smart_link`, resolved back to
/// the `smart_links` row that serves it. A published post with no tracked
/// link is unmeasurable rather than a zero: the draft named a destination
/// no redirect was minted for, and "the link was never instrumented" is
/// not "nobody clicked".
pub(super) async fn content_link_clicks(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    let tracked = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM (
            SELECT post.smart_link_id AS link_id
            FROM social_posts AS post
            WHERE post.workspace_id = $1 AND post.action_id = $2
              AND post.smart_link_id IS NOT NULL
            UNION
            SELECT post.smart_link_id
            FROM telegram_posts AS post
            WHERE post.workspace_id = $1 AND post.action_id = $2
              AND post.smart_link_id IS NOT NULL
            UNION
            SELECT post.smart_link_id
            FROM discord_posts AS post
            WHERE post.workspace_id = $1 AND post.action_id = $2
              AND post.smart_link_id IS NOT NULL
            UNION
            SELECT link.id
            FROM community_posts AS post
            JOIN smart_links AS link
              ON link.workspace_id = post.workspace_id
             AND link.slug = substring(post.smart_link from 4)
            WHERE post.workspace_id = $1 AND post.action_id = $2
              AND post.smart_link LIKE '/l/%'
        ) AS links
        "#,
    )
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
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COUNT(*)::double precision
        FROM click_events AS click
        WHERE click.workspace_id = $1
          AND click.occurred_at >= $3
          AND click.occurred_at < $3 + INTERVAL '7 days'
          AND click.smart_link_id IN (
              SELECT post.smart_link_id
              FROM social_posts AS post
              WHERE post.workspace_id = $1 AND post.action_id = $2
                AND post.smart_link_id IS NOT NULL
              UNION
              SELECT post.smart_link_id
              FROM telegram_posts AS post
              WHERE post.workspace_id = $1 AND post.action_id = $2
                AND post.smart_link_id IS NOT NULL
              UNION
              SELECT post.smart_link_id
              FROM discord_posts AS post
              WHERE post.workspace_id = $1 AND post.action_id = $2
                AND post.smart_link_id IS NOT NULL
              UNION
              SELECT link.id
              FROM community_posts AS post
              JOIN smart_links AS link
                ON link.workspace_id = post.workspace_id
               AND link.slug = substring(post.smart_link from 4)
              WHERE post.workspace_id = $1 AND post.action_id = $2
                AND post.smart_link LIKE '/l/%'
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .bind(measurement.action_finished_at)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}


/// Fans acquired through this action's tracked content link.
///
/// The join is deliberately visitor-bound: a click on the action's smart link
/// carries the anonymous visitor id into the acquisition ledger. Counting the
/// workspace's whole fan delta would credit unrelated releases, imports or
/// referrals to the post. Distinct fan ids also prevent repeat clicks from
/// multiplying one signup.
pub(super) async fn content_fan_acquisitions(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<f64, RepositoryError> {
    let tracked = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM (
            SELECT post.smart_link_id AS link_id
            FROM social_posts AS post
            WHERE post.workspace_id = $1 AND post.action_id = $2
              AND post.smart_link_id IS NOT NULL
            UNION
            SELECT post.smart_link_id
            FROM telegram_posts AS post
            WHERE post.workspace_id = $1 AND post.action_id = $2
              AND post.smart_link_id IS NOT NULL
            UNION
            SELECT post.smart_link_id
            FROM discord_posts AS post
            WHERE post.workspace_id = $1 AND post.action_id = $2
              AND post.smart_link_id IS NOT NULL
            UNION
            SELECT link.id
            FROM community_posts AS post
            JOIN smart_links AS link
              ON link.workspace_id = post.workspace_id
             AND link.slug = substring(post.smart_link from 4)
            WHERE post.workspace_id = $1 AND post.action_id = $2
              AND post.smart_link LIKE '/l/%'
        ) AS links
        "#,
    )
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

    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT COUNT(DISTINCT acquisition.fan_id)::double precision
        FROM click_events AS click
        JOIN fan_acquisition_events AS acquisition
          ON acquisition.workspace_id = click.workspace_id
         AND acquisition.anonymous_visitor_id = click.anonymous_visitor_id
         AND acquisition.occurred_at >= click.occurred_at
         AND acquisition.occurred_at < $3 + INTERVAL '7 days'
        WHERE click.workspace_id = $1
          AND click.occurred_at >= $3
          AND click.occurred_at < $3 + INTERVAL '7 days'
          AND click.anonymous_visitor_id IS NOT NULL
          AND click.smart_link_id IN (
              SELECT post.smart_link_id
              FROM social_posts AS post
              WHERE post.workspace_id = $1 AND post.action_id = $2
                AND post.smart_link_id IS NOT NULL
              UNION
              SELECT post.smart_link_id
              FROM telegram_posts AS post
              WHERE post.workspace_id = $1 AND post.action_id = $2
                AND post.smart_link_id IS NOT NULL
              UNION
              SELECT post.smart_link_id
              FROM discord_posts AS post
              WHERE post.workspace_id = $1 AND post.action_id = $2
                AND post.smart_link_id IS NOT NULL
              UNION
              SELECT link.id
              FROM community_posts AS post
              JOIN smart_links AS link
                ON link.workspace_id = post.workspace_id
               AND link.slug = substring(post.smart_link from 4)
              WHERE post.workspace_id = $1 AND post.action_id = $2
                AND post.smart_link LIKE '/l/%'
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .bind(measurement.action_finished_at)
    .fetch_one(pool)
    .await
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
