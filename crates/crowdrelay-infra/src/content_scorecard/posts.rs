//! The scorecard's post ledger — which of a video's posts reached an
//! audience, and where each sits in the publication-measurement lifecycle.
//!
//! Split out of `content_scorecard` so both stay inside the source-size
//! ratchet. The shared `source_posts` union is the one place "a post belongs
//! to this video" is defined; both readers count over it.

use std::collections::HashMap;

use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::{PublicationStageSummary, VideoPostCounts};
use sqlx::PgPool;
use uuid::Uuid;

use super::{VIDEO_LINKS_CTE, map_sqlx};

/// The per-lane send counts one video's posts resolve to.
#[derive(Clone, Debug, Default)]
pub(super) struct LanePostLedgers {
    pub(super) community: VideoPostCounts,
    pub(super) telegram: VideoPostCounts,
    pub(super) discord: VideoPostCounts,
    pub(super) social: VideoPostCounts,
}

/// A post belongs to a video through `relay_source_id`, its action's payload
/// `source_id`, or a bound link whose destination is the video — three
/// writers, three join shapes. The shared union emits the post columns both
/// readers need (status, publication clock, owning action) and DISTINCT keeps
/// a post that matched two paths at one row per (lane, post, video).
// A WITH-clause fragment, not a statement — the `videos` and `video_links`
// names it joins are CTEs its composed callers bind via `VIDEO_LINKS_CTE`,
// so it stays a plain string.
const SOURCE_POSTS_CTE: &str = "
    , source_posts AS (
        SELECT DISTINCT lane, post_id, source_id, action_id, status, posted_at
        FROM (
            SELECT 'community' AS lane, post.id AS post_id,
                   v.id AS source_id, post.action_id, post.status, post.posted_at
            FROM community_posts post
            JOIN videos v ON v.id = post.relay_source_id
            WHERE post.workspace_id = $1
            UNION ALL
            SELECT 'community', post.id, v.id, post.action_id, post.status, post.posted_at
            FROM community_posts post
            JOIN autopilot_actions a
              ON a.workspace_id = post.workspace_id
             AND a.id = post.action_id
            JOIN videos v ON v.id::text = COALESCE(a.payload ->> 'source_id', a.payload -> 'draft' ->> 'source_id')
            WHERE post.workspace_id = $1
            UNION ALL
            SELECT 'community', post.id, v.source_id, post.action_id, post.status, post.posted_at
            FROM community_posts post
            JOIN smart_links link
              ON link.workspace_id = post.workspace_id
             AND post.smart_link = '/l/' || link.slug
            JOIN video_links v ON v.id = link.id
            WHERE post.workspace_id = $1
            UNION ALL
            SELECT 'telegram', post.id, v.id, post.action_id, post.status, post.posted_at
            FROM telegram_posts post
            JOIN autopilot_actions a
              ON a.workspace_id = post.workspace_id
             AND a.id = post.action_id
            JOIN videos v ON v.id::text = COALESCE(a.payload ->> 'source_id', a.payload -> 'draft' ->> 'source_id')
            WHERE post.workspace_id = $1
            UNION ALL
            SELECT 'telegram', post.id, v.source_id, post.action_id, post.status, post.posted_at
            FROM telegram_posts post
            JOIN video_links v ON v.id = post.smart_link_id
            WHERE post.workspace_id = $1
            UNION ALL
            SELECT 'discord', post.id, v.id, post.action_id, post.status, post.posted_at
            FROM discord_posts post
            JOIN autopilot_actions a
              ON a.workspace_id = post.workspace_id
             AND a.id = post.action_id
            JOIN videos v ON v.id::text = COALESCE(a.payload ->> 'source_id', a.payload -> 'draft' ->> 'source_id')
            WHERE post.workspace_id = $1
            UNION ALL
            SELECT 'discord', post.id, v.source_id, post.action_id, post.status, post.posted_at
            FROM discord_posts post
            JOIN video_links v ON v.id = post.smart_link_id
            WHERE post.workspace_id = $1
            UNION ALL
            SELECT 'social', post.id, v.id, post.action_id, post.status, post.posted_at
            FROM social_posts post
            JOIN autopilot_actions a
              ON a.workspace_id = post.workspace_id
             AND a.id = post.action_id
            JOIN videos v ON v.id::text = COALESCE(a.payload ->> 'source_id', a.payload -> 'draft' ->> 'source_id')
            WHERE post.workspace_id = $1
            UNION ALL
            SELECT 'social', post.id, v.source_id, post.action_id, post.status, post.posted_at
            FROM social_posts post
            JOIN video_links v ON v.id = post.smart_link_id
            WHERE post.workspace_id = $1
        ) hits
    )
";

/// The send ledger's post half — per-lane counts over [`SOURCE_POSTS_CTE`].
pub(super) async fn post_ledgers(
    pool: &PgPool,
    ws: Uuid,
    source_ids: &[Uuid],
) -> Result<HashMap<Uuid, LanePostLedgers>, RepositoryError> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, i64)>(&format!(
        "{VIDEO_LINKS_CTE}{SOURCE_POSTS_CTE}
            SELECT h.source_id, h.lane, h.status, count(*)
            FROM source_posts h
            GROUP BY h.source_id, h.lane, h.status
            ",
    ))
    .bind(ws)
    .bind(source_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let mut ledgers: HashMap<Uuid, LanePostLedgers> = HashMap::new();
    for (source_id, lane, status, count) in rows {
        let counts = ledgers.entry(source_id).or_default();
        let lane_counts = match lane.as_str() {
            "community" => &mut counts.community,
            "telegram" => &mut counts.telegram,
            "discord" => &mut counts.discord,
            _ => &mut counts.social,
        };
        let count = count.max(0) as u64;
        match status.as_str() {
            "posted" => lane_counts.posted = count,
            "failed" | "cancelled" => lane_counts.failed = count,
            "awaiting_manual_post" => {
                lane_counts.awaiting_manual_post = count;
                lane_counts.waiting += count;
            }
            _ => lane_counts.waiting += count,
        }
    }
    Ok(ledgers)
}

/// The publication-measurement lifecycle per video — the same stage CASE the
/// ops queue aggregates, counted per source instead of per channel. Each of
/// the video's posts lands in exactly one stage; `accepted` counts the posts
/// whose `autopilot_outcomes` row the learner already holds. A card where
/// every post reads `no_measurement` or `awaiting_publication` has told the
/// brain nothing yet — and must not look like a zero.
pub(super) async fn measurement_stages(
    pool: &PgPool,
    ws: Uuid,
    source_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<PublicationStageSummary>>, RepositoryError> {
    let stage = crate::publication_stage::PUBLICATION_STAGE_SQL;
    let rows = sqlx::query_as::<_, (Uuid, String, i64, i64)>(&format!(
        "{VIDEO_LINKS_CTE}{SOURCE_POSTS_CTE}
            SELECT staged.source_id, staged.stage, count(*),
                   count(*) FILTER (WHERE staged.accepted)
            FROM (
                SELECT DISTINCT ON (post.post_id)
                       post.source_id, ({stage}) AS stage,
                       outcome.id IS NOT NULL AS accepted
                FROM source_posts post
                LEFT JOIN autopilot_measurements AS measurement
                  ON measurement.workspace_id = $1
                 AND measurement.action_id = post.action_id
                 AND measurement.measurement_kind IN
                     ('content_fan_acquisition_7d', 'content_link_clicks_7d')
                LEFT JOIN autopilot_outcomes AS outcome
                  ON outcome.workspace_id = measurement.workspace_id
                 AND outcome.measurement_id = measurement.id
                ORDER BY post.post_id,
                         CASE measurement.measurement_kind
                             WHEN 'content_fan_acquisition_7d' THEN 0 ELSE 1
                         END
            ) staged
            GROUP BY staged.source_id, staged.stage
            ",
    ))
    .bind(ws)
    .bind(source_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let mut stages: HashMap<Uuid, Vec<PublicationStageSummary>> = HashMap::new();
    for (source_id, stage, posts, accepted) in rows {
        stages
            .entry(source_id)
            .or_default()
            .push(PublicationStageSummary {
                stage,
                posts: posts.max(0) as u64,
                accepted: accepted.max(0) as u64,
            });
    }
    Ok(stages)
}
