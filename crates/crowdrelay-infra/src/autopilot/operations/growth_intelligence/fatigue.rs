//! The band's own community posting history, spaced, for the fatigue measure
//! (`crowdrelay_brain::fatigue`): each published post's community, when it
//! went up, and its latest score.
//!
//! Posts under two days old are left out — their score has not settled, and
//! an unsettled quick follow-up would read as fatigue that is only youth.

use crowdrelay_brain::fatigue::{FatigueMeasure, SpacedPost, measure_fatigue};
use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;

use super::{RepositoryError, map_sqlx};

pub(super) async fn load_fatigue(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Option<FatigueMeasure>, RepositoryError> {
    let rows: Vec<(String, f64, i32)> = sqlx::query_as(
        r#"
        SELECT normalize_subreddit(cp.subreddit),
               (EXTRACT(EPOCH FROM cp.posted_at) / 86400.0)::double precision,
               latest.score
        FROM community_posts cp
        JOIN LATERAL (
            SELECT m.score
            FROM community_post_metrics m
            WHERE m.workspace_id = cp.workspace_id
              AND m.community_post_id = cp.id
            ORDER BY m.measured_at DESC
            LIMIT 1
        ) latest ON true
        WHERE cp.workspace_id = $1
          AND cp.status = 'posted'
          AND cp.posted_at > now() - interval '180 days'
          AND cp.posted_at < now() - interval '2 days'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    let posts: Vec<SpacedPost> = rows
        .into_iter()
        .map(|(audience, posted_day, score)| SpacedPost {
            audience,
            posted_day,
            score: f64::from(score),
        })
        .collect();
    Ok(measure_fatigue(&posts))
}
