//! Reads the threads the community sweep recorded, for the demand scout.
//!
//! `fan_observations` rows of kind `post` are what the sweep saw in each room
//! on each pass: the thread's title as `fact`, its permalink as `url`, and its
//! score/comments/flair as `metrics`. The sweep revisits a room every pass, so
//! the same thread appears many times; this read collapses them to the newest
//! reading per URL.
//!
//! The statement is workspace-scoped on both tables it names — the tenant
//! isolation ratchet counts a read of a `workspace_id` table that does not.

use sqlx::{FromRow, PgPool};
use time::Date;
use uuid::Uuid;

#[derive(Debug, FromRow)]
pub struct RoomThreadRow {
    pub place_id: Uuid,
    pub room: String,
    pub membership_state: String,
    pub url: String,
    pub title: String,
    pub flair: Option<String>,
    pub comments: Option<i32>,
    /// The thread's own date (`fan_observations.observed_at` is a `date`).
    pub posted_on: Date,
}

/// The newest reading of every thread seen since `since`, in rooms the band
/// has not ruled out. `not_a_fit` and `rejected` rooms are excluded: a thread
/// in a room that refused the band, or that the band judged not its audience,
/// is not an invitation.
///
/// Bounded by `limit` (the newest first) so a busy week cannot turn this into
/// an unbounded scan.
pub async fn recent_room_threads(
    pool: &PgPool,
    workspace_id: Uuid,
    since: Date,
    limit: i64,
) -> Result<Vec<RoomThreadRow>, sqlx::Error> {
    sqlx::query_as::<_, RoomThreadRow>(
        r#"
        SELECT * FROM (
            SELECT DISTINCT ON (o.url)
                   o.place_id,
                   p.name AS room,
                   p.membership_state,
                   o.url,
                   o.fact AS title,
                   NULLIF(o.metrics->>'flair', '') AS flair,
                   CASE WHEN jsonb_typeof(o.metrics->'comments') = 'number'
                        THEN (o.metrics->>'comments')::int END AS comments,
                   o.observed_at AS posted_on
            FROM fan_observations o
            JOIN discovery_places p
              ON p.workspace_id = o.workspace_id AND p.id = o.place_id
            WHERE o.workspace_id = $1
              AND o.kind = 'post'
              AND o.observed_at >= $2
              AND o.url IS NOT NULL
              AND p.membership_state NOT IN ('not_a_fit', 'rejected')
            ORDER BY o.url, o.observed_at DESC, o.captured_at DESC, o.id DESC
        ) newest
        ORDER BY posted_on DESC, url
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await
}
