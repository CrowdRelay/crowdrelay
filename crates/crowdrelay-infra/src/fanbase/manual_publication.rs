//! Registering a post an operator published by hand.
//!
//! Split out of `fanbase.rs` to keep both inside the source-size ratchet, and
//! because it is one job: an operator published something the system drafted,
//! and every record the automatic path would have written has to be written
//! now instead.
//!
//! That last part is the whole reason this is delicate. Reddit is read-only by
//! policy and the other channels default to manual, so this is the path a real
//! post actually takes — and anything the automatic path writes that this one
//! does not is a difference between what happened and what the brain learns.

use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Manual Reddit post registration
//
// Registers a manually-posted Reddit URL for a community post that was
// in `awaiting_manual_post` status. Extracts the Reddit post ID from
// the URL and transitions the row to `posted` so the metrics poller
// can track it.
// ---------------------------------------------------------------------------

/// Which of the two reasons a zero-row update had.
///
/// Runs only on the failure path, inside the same transaction — the update
/// affected nothing, so there is no work to roll back. The table name is
/// interpolated from a fixed set of callers in this module and never from
/// input.
async fn publication_failure(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &'static str,
    workspace_id: Uuid,
    post_id: Uuid,
) -> Result<PublicationFailure, sqlx::Error> {
    let status: Option<String> = sqlx::query_scalar(&format!(
        "SELECT status FROM {table} WHERE id = $1 AND workspace_id = $2"
    ))
    .bind(post_id)
    .bind(workspace_id)
    .fetch_optional(&mut **transaction)
    .await?
    .flatten();
    Ok(match status {
        Some(status) => PublicationFailure::WrongStatus(status),
        None => PublicationFailure::Missing,
    })
}

/// Why a manual registration could not be applied.
enum PublicationFailure {
    Missing,
    WrongStatus(String),
}

/// Error type for manual Reddit post registration.
#[derive(Debug, thiserror::Error)]
pub enum ManualRedditPostError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// No row with that id in this workspace.
    ///
    /// Split from the status case because the caller answers them differently
    /// and an operator acts on them differently. Both used to be `NotFound`,
    /// and the API layer turned every variant here into 400 — so publishing a
    /// post by hand and then registering it answered "the request could not be
    /// parsed" whether the id was wrong, the post was already registered, or
    /// the database was down. Reddit is manual by policy, so this is the path
    /// every real post takes, and the natural response to an ambiguous failure
    /// is to retry or to re-post.
    #[error("post not found in this workspace")]
    NotFound,
    /// The row exists and is not waiting to be published.
    ///
    /// Almost always because it is already `posted` — the operator registered
    /// it once and is retrying. Carrying the status lets the answer say so
    /// instead of leaving them to guess.
    #[error("post is {status}, not awaiting_manual_post")]
    NotAwaitingPublication { status: String },
}

/// Registers a manually-posted Reddit URL for a community post that was
/// drafted by the system but posted manually by the operator (manual mode).
/// Transitions the post to `posted` status so the metrics poller can track
/// its performance via Reddit's public JSON endpoint.
///
/// # Errors
/// Returns [`ManualRedditPostError::NotFound`] if the post doesn't exist or
/// isn't in `awaiting_manual_post` status.
pub async fn register_manual_reddit_post(
    pool: &PgPool,
    workspace_id: Uuid,
    community_post_id: Uuid,
    reddit_post_url: &str,
) -> Result<(), ManualRedditPostError> {
    let reddit_post_id = extract_reddit_post_id(reddit_post_url).ok_or_else(|| {
        ManualRedditPostError::InvalidUrl(format!(
            "could not extract post ID from URL: {reddit_post_url}"
        ))
    })?;

    let mut transaction = pool.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE community_posts
        SET status = 'posted',
            reddit_post_id = $3,
            reddit_post_url = $4,
            posted_at = now(),
            updated_at = now(),
            error_message = NULL
        WHERE id = $1
          AND workspace_id = $2
          AND status = 'awaiting_manual_post'
        "#,
    )
    .bind(community_post_id)
    .bind(workspace_id)
    .bind(&reddit_post_id)
    .bind(reddit_post_url)
    .execute(&mut *transaction)
    .await?;

    if result.rows_affected() == 0 {
        return Err(
            match publication_failure(
                &mut transaction,
                "community_posts",
                workspace_id,
                community_post_id,
            )
            .await?
            {
                PublicationFailure::Missing => ManualRedditPostError::NotFound,
                PublicationFailure::WrongStatus(status) => {
                    ManualRedditPostError::NotAwaitingPublication { status }
                }
            },
        );
    }
    anchor_measurements_to_publication(&mut *transaction, workspace_id, community_post_id).await?;
    record_publication_reach(&mut transaction, workspace_id, community_post_id).await?;
    transaction.commit().await?;
    Ok(())
}

/// Records the reach and the execution status of a manually published post.
///
/// The automatic path writes both the moment Reddit confirms the post; this
/// path wrote neither, and Reddit is manual by policy — so every Reddit post
/// this tenant has ever really made was missing both.
///
/// What that cost:
///
/// - **Reach.** `viryaos_reach_events` is the denominator the credit allocator
///   divides fan outcomes by. A post with no reach row is a post that reached
///   nobody as far as attribution is concerned, so a community that genuinely
///   converted someone could not be credited for it.
/// - **Execution status.** The experiment assignment stayed `dispatched`
///   forever. The learner reads that column to decide which rows contribute
///   to the treatment-effect posterior, so a unit that was treated in the
///   world was not a treated unit in the model.
///
/// Both are written in the same transaction as the publication itself: a post
/// that is `posted` and has no reach row is the state this exists to prevent,
/// and two statements that can disagree are worse than one that cannot.
///
/// The same estimate the automatic path uses -- 100 for a subreddit broadcast.
/// Not a measurement, and identical in both paths on purpose: a manual post
/// and an automatic one to the same community must not be worth different
/// amounts because of who pressed publish.
async fn record_publication_reach(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    community_post_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_reach_events (
            workspace_id, action_id, recipient_kind, recipient_id, channel,
            template_id, estimated_reach, status, metadata, trace_id, causation_id
        )
        SELECT $1, post.action_id, 'subreddit_audience', post.subreddit, 'reddit_post',
               'community-engager', 100, 'delivered',
               jsonb_build_object('subreddit', post.subreddit,
                                  'post_url', post.reddit_post_url,
                                  'published', 'manual'),
               action.trace_id, post.action_id
        FROM community_posts AS post
        JOIN viryaos_autopilot_actions AS action
          ON action.workspace_id = post.workspace_id AND action.id = post.action_id
        WHERE post.workspace_id = $1 AND post.id = $2
        ON CONFLICT (action_id, recipient_id, channel)
            WHERE action_id IS NOT NULL DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(community_post_id)
    .execute(&mut **transaction)
    .await?;

    // Monotonic, exactly as the automatic path is: only dispatched → executed,
    // so re-registering a URL cannot walk the status backwards.
    sqlx::query(
        r#"
        UPDATE viryaos_experiment_assignments AS assignment
        SET execution_status = 'executed',
            trace_id = COALESCE(assignment.trace_id, action.trace_id)
        FROM community_posts AS post
        JOIN viryaos_autopilot_actions AS action
          ON action.workspace_id = post.workspace_id AND action.id = post.action_id
        WHERE assignment.workspace_id = $1
          AND post.workspace_id = $1
          AND post.id = $2
          AND assignment.action_id = post.action_id
          AND assignment.execution_status = 'dispatched'
        "#,
    )
    .bind(workspace_id)
    .bind(community_post_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Moves the measurement windows for a manually published post so they start
/// when the post reached the community.
///
/// A draft is not an exposure. The windows were anchored to
/// `action_finished_at`, which for the manual flow is the moment the text was
/// written — and an operator who publishes on Thursday what the agent drafted
/// on Monday would have had three of the fourteen days spent measuring a world
/// the post was not in yet. Worse, the days are not neutral: they carry
/// whatever the rest of the workspace did, credited to a post nobody had seen.
///
/// Two actions are re-anchored. The post's own action carries the engagement
/// measurement; the agent run that drafted it carries Y14 and Y30. They are
/// joined through the community itself — the draft names the target, and the
/// experiment assigned that same target to the dispatch that produced it. The
/// most recent assignment for the target before the draft existed is that
/// dispatch, which keeps an older post to the same community from dragging an
/// earlier dispatch's windows forward with it.
///
/// Each measurement keeps its own offset — seven days stays seven days from
/// publication, forty-four stays forty-four — so only the origin moves.
/// Re-anchors a published community post's pending measurements from
/// dispatch time to `posted_at`. Generic over the executor so the manual
/// registration path runs it inside its transaction and the community
/// executor can run it on a bare pool connection after stamping `posted`.
pub async fn anchor_measurements_to_publication<'e, E>(
    executor: E,
    workspace_id: Uuid,
    community_post_id: Uuid,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        r#"
        WITH published AS (
            SELECT post.action_id, post.target_id, post.created_at, post.posted_at
            FROM community_posts AS post
            WHERE post.workspace_id = $1 AND post.id = $2
        ), owning_actions AS (
            SELECT published.action_id, published.posted_at
            FROM published
            UNION
            SELECT assignment.action_id, published.posted_at
            FROM published
            JOIN LATERAL (
                SELECT candidate.action_id
                FROM viryaos_experiment_assignments AS candidate
                WHERE candidate.workspace_id = $1
                  AND candidate.unit_kind = 'target_community'
                  AND candidate.unit_id = published.target_id::text
                  AND candidate.action_id IS NOT NULL
                  AND candidate.assigned_at <= published.created_at
                ORDER BY candidate.assigned_at DESC
                LIMIT 1
            ) AS assignment ON true
            WHERE published.target_id IS NOT NULL
        )
        UPDATE viryaos_autopilot_measurements AS measurement
        SET action_finished_at = owning_actions.posted_at,
            due_at = owning_actions.posted_at
                     + (measurement.due_at - measurement.action_finished_at),
            available_at = owning_actions.posted_at
                     + (measurement.due_at - measurement.action_finished_at)
        FROM owning_actions
        WHERE measurement.workspace_id = $1
          AND measurement.action_id = owning_actions.action_id
          AND measurement.status = 'pending'
          AND measurement.action_finished_at < owning_actions.posted_at
        "#,
    )
    .bind(workspace_id)
    .bind(community_post_id)
    .execute(executor)
    .await?;
    Ok(())
}

/// Extracts the Reddit post ID from a URL like:
/// `https://www.reddit.com/r/subreddit/comments/abc123/title/` → `abc123`
fn extract_reddit_post_id(url: &str) -> Option<String> {
    let parts: Vec<&str> = url.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "comments"
            && let Some(id) = parts.get(i + 1)
            && !id.is_empty()
        {
            return Some((*id).to_owned());
        }
    }
    None
}

/// Error type for manual content post registration (social/telegram/discord).
#[derive(Debug, thiserror::Error)]
pub enum ManualContentPostError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// No row with that id in this workspace.
    ///
    /// Split from the status case because the caller answers them differently
    /// and an operator acts on them differently. Both used to be `NotFound`,
    /// and the API layer turned every variant here into 400 — so publishing a
    /// post by hand and then registering it answered "the request could not be
    /// parsed" whether the id was wrong, the post was already registered, or
    /// the database was down. Reddit is manual by policy, so this is the path
    /// every real post takes, and the natural response to an ambiguous failure
    /// is to retry or to re-post.
    #[error("post not found in this workspace")]
    NotFound,
    /// The row exists and is not waiting to be published.
    ///
    /// Almost always because it is already `posted` — the operator registered
    /// it once and is retrying. Carrying the status lets the answer say so
    /// instead of leaving them to guess.
    #[error("post is {status}, not awaiting_manual_post")]
    NotAwaitingPublication { status: String },
}

/// Registers a manually-posted social post URL for a social post that was
/// drafted by the system but posted manually by the operator (manual mode).
/// Transitions the post to `posted` status so the receipt reconciliation
/// sweep can resolve the parent autopilot action as provider-confirmed.
///
/// # Errors
/// Returns [`ManualContentPostError::NotFound`] if the post doesn't exist or
/// isn't in `awaiting_manual_post` status.
pub async fn register_manual_social_post(
    pool: &PgPool,
    workspace_id: Uuid,
    social_post_id: Uuid,
    platform_post_url: &str,
    platform_post_id: Option<&str>,
) -> Result<(), ManualContentPostError> {
    let mut transaction = pool.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE social_posts
        SET status = 'posted',
            platform_post_url = $3,
            platform_post_id = $4,
            posted_at = now(),
            updated_at = now(),
            error_message = NULL
        WHERE id = $1
          AND workspace_id = $2
          AND status = 'awaiting_manual_post'
        "#,
    )
    .bind(social_post_id)
    .bind(workspace_id)
    .bind(platform_post_url)
    .bind(platform_post_id)
    .execute(&mut *transaction)
    .await?;

    if result.rows_affected() == 0 {
        return Err(
            match publication_failure(
                &mut transaction,
                "social_posts",
                workspace_id,
                social_post_id,
            )
            .await?
            {
                PublicationFailure::Missing => ManualContentPostError::NotFound,
                PublicationFailure::WrongStatus(status) => {
                    ManualContentPostError::NotAwaitingPublication { status }
                }
            },
        );
    }
    anchor_content_measurements_to_publication(
        &mut transaction,
        workspace_id,
        "social_posts",
        social_post_id,
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// Registers a manually-posted Telegram message for a Telegram post that was
/// drafted by the system but posted manually by the operator (manual mode).
/// Transitions the post to `posted` status so the receipt reconciliation
/// sweep can resolve the parent autopilot action as provider-confirmed.
///
/// # Errors
/// Returns [`ManualContentPostError::NotFound`] if the post doesn't exist or
/// isn't in `awaiting_manual_post` status.
pub async fn register_manual_telegram_post(
    pool: &PgPool,
    workspace_id: Uuid,
    telegram_post_id: Uuid,
    message_id: i64,
) -> Result<(), ManualContentPostError> {
    let mut transaction = pool.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE telegram_posts
        SET status = 'posted',
            message_id = $3,
            posted_at = now(),
            updated_at = now(),
            error_message = NULL
        WHERE id = $1
          AND workspace_id = $2
          AND status = 'awaiting_manual_post'
        "#,
    )
    .bind(telegram_post_id)
    .bind(workspace_id)
    .bind(message_id)
    .execute(&mut *transaction)
    .await?;

    if result.rows_affected() == 0 {
        return Err(
            match publication_failure(
                &mut transaction,
                "telegram_posts",
                workspace_id,
                telegram_post_id,
            )
            .await?
            {
                PublicationFailure::Missing => ManualContentPostError::NotFound,
                PublicationFailure::WrongStatus(status) => {
                    ManualContentPostError::NotAwaitingPublication { status }
                }
            },
        );
    }
    anchor_content_measurements_to_publication(
        &mut transaction,
        workspace_id,
        "telegram_posts",
        telegram_post_id,
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// Registers a manually-posted Discord message for a Discord post that was
/// drafted by the system but posted manually by the operator (manual mode).
/// Transitions the post to `posted` status so the receipt reconciliation
/// sweep can resolve the parent autopilot action as provider-confirmed.
///
/// # Errors
/// Returns [`ManualContentPostError::NotFound`] if the post doesn't exist or
/// isn't in `awaiting_manual_post` status.
pub async fn register_manual_discord_post(
    pool: &PgPool,
    workspace_id: Uuid,
    discord_post_id: Uuid,
    message_id: &str,
) -> Result<(), ManualContentPostError> {
    let mut transaction = pool.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE discord_posts
        SET status = 'posted',
            message_id = $3,
            posted_at = now(),
            updated_at = now(),
            error_message = NULL
        WHERE id = $1
          AND workspace_id = $2
          AND status = 'awaiting_manual_post'
        "#,
    )
    .bind(discord_post_id)
    .bind(workspace_id)
    .bind(message_id)
    .execute(&mut *transaction)
    .await?;

    if result.rows_affected() == 0 {
        return Err(
            match publication_failure(
                &mut transaction,
                "discord_posts",
                workspace_id,
                discord_post_id,
            )
            .await?
            {
                PublicationFailure::Missing => ManualContentPostError::NotFound,
                PublicationFailure::WrongStatus(status) => {
                    ManualContentPostError::NotAwaitingPublication { status }
                }
            },
        );
    }
    anchor_content_measurements_to_publication(
        &mut transaction,
        workspace_id,
        "discord_posts",
        discord_post_id,
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// Moves the measurement windows for a manually published content post so
/// they start when the post reached the audience. Mirrors
/// `anchor_measurements_to_publication` for community posts but works for
/// social_posts, telegram_posts, and discord_posts — all of which carry an
/// `action_id` column that links back to the autopilot action.
///
/// Public so the automatic publish paths in the channel executors can run
/// the same re-anchor inside their mark-posted transaction — a post that
/// sat `rate_limited` must not be observed across a window that started
/// before anyone could see it.
pub async fn anchor_content_measurements_to_publication(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    table: &str,
    post_id: Uuid,
) -> Result<(), sqlx::Error> {
    // The table name is a compile-time constant from the caller, not user
    // input, so interpolation is safe here.
    let query = format!(
        r#"
        WITH published AS (
            SELECT post.action_id, post.posted_at
            FROM {table} AS post
            WHERE post.workspace_id = $1 AND post.id = $2
        )
        UPDATE viryaos_autopilot_measurements AS measurement
        SET action_finished_at = published.posted_at,
            due_at = published.posted_at
                     + (measurement.due_at - measurement.action_finished_at),
            available_at = published.posted_at
                     + (measurement.due_at - measurement.action_finished_at)
        FROM published
        WHERE measurement.workspace_id = $1
          AND measurement.action_id = published.action_id
          AND measurement.status = 'pending'
          AND measurement.action_finished_at < published.posted_at
        "#,
    );
    sqlx::query(&query)
        .bind(workspace_id)
        .bind(post_id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_reddit_post_id_finds_id() {
        assert_eq!(
            extract_reddit_post_id("https://www.reddit.com/r/Metal/comments/abc123/title/"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn extract_reddit_post_id_returns_none_for_non_reddit_url() {
        assert_eq!(
            extract_reddit_post_id("https://example.com/no/comments"),
            None
        );
    }
}
