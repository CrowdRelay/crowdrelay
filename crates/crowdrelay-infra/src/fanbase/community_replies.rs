//! The operator's side of the reply lane: what the band is about to say to
//! the people who commented on its posts, and the three answers a person can
//! give — send it, send it as edited, or don't.
//!
//! The worker owns everything else (harvest, draft, review, send). An
//! approval here only moves a row to `approved` with a fresh randomised
//! `not_before`; the worker still applies the account's standing, the daily
//! ceiling and the spacing before anything leaves.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// One comment and the band's drafted answer, as the queue shows it.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CommunityReplyView {
    pub id: Uuid,
    pub subreddit: String,
    pub post_title: String,
    pub post_url: Option<String>,
    pub author: String,
    pub comment: String,
    pub status: String,
    pub draft: Option<String>,
    /// Why it is waiting: a guard, the review, a halted account. `None` when
    /// it only waits because unattended replies are off.
    pub hold_reason: Option<String>,
    pub review_score: Option<i16>,
    pub reply_permalink: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Waiting drafts first, then the last week's answered and skipped ones.
pub async fn list_community_replies(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<CommunityReplyView>, sqlx::Error> {
    sqlx::query_as::<_, CommunityReplyView>(
        r#"
        SELECT c.id, p.subreddit, p.title AS post_title, p.reddit_post_url AS post_url,
               c.author, c.body AS comment, c.status, c.draft, c.hold_reason,
               c.review_score, c.reply_permalink, c.created_at
        FROM community_comments c
        JOIN community_posts p ON p.id = c.community_post_id AND p.workspace_id = c.workspace_id
        WHERE c.workspace_id = $1
          AND (c.status IN ('awaiting_approval', 'approved', 'unanswered')
               OR c.updated_at > now() - INTERVAL '7 days')
        ORDER BY (c.status = 'awaiting_approval') DESC, c.created_at DESC
        LIMIT 100
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await
}

#[derive(Debug, thiserror::Error)]
pub enum CommunityReplyError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("no such reply in this workspace")]
    NotFound,
    #[error("the reply is not waiting for an answer (status {0})")]
    NotAwaiting(String),
    #[error("the edited reply is empty or longer than 400 characters")]
    InvalidDraft,
}

/// Longest reply a person may approve — the drafter's own bound.
const MAX_REPLY_CHARS: usize = 400;

/// Approves a waiting draft, optionally as edited. The worker sends it after a
/// randomised delay, under the account's standing and the reply ceiling.
pub async fn approve_community_reply(
    pool: &PgPool,
    workspace_id: Uuid,
    reply_id: Uuid,
    edited: Option<&str>,
    approved_by: &str,
    not_before: OffsetDateTime,
) -> Result<(), CommunityReplyError> {
    let edited = match edited.map(str::trim) {
        Some(text) if text.is_empty() || text.chars().count() > MAX_REPLY_CHARS => {
            return Err(CommunityReplyError::InvalidDraft);
        }
        other => other,
    };
    let updated = sqlx::query(
        r#"
        UPDATE community_comments
        SET status = 'approved',
            draft = COALESCE($3, draft),
            approved_by = $4,
            not_before = $5,
            hold_reason = NULL,
            updated_at = now()
        WHERE workspace_id = $1 AND id = $2
          AND status = 'awaiting_approval'
          AND COALESCE($3, draft) IS NOT NULL
        "#,
    )
    .bind(workspace_id)
    .bind(reply_id)
    .bind(edited)
    .bind(approved_by.chars().take(200).collect::<String>())
    .bind(not_before)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 1 {
        return Ok(());
    }
    Err(status_error(pool, workspace_id, reply_id).await)
}

/// Declines to answer. Kept, not deleted: "what did we choose not to say" is
/// a question worth being able to answer.
pub async fn skip_community_reply(
    pool: &PgPool,
    workspace_id: Uuid,
    reply_id: Uuid,
) -> Result<(), CommunityReplyError> {
    let updated = sqlx::query(
        r#"
        UPDATE community_comments
        SET status = 'skipped', hold_reason = 'skipped by an operator', updated_at = now()
        WHERE workspace_id = $1 AND id = $2
          AND status IN ('unanswered', 'awaiting_approval', 'approved')
        "#,
    )
    .bind(workspace_id)
    .bind(reply_id)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 1 {
        return Ok(());
    }
    Err(status_error(pool, workspace_id, reply_id).await)
}

async fn status_error(pool: &PgPool, workspace_id: Uuid, reply_id: Uuid) -> CommunityReplyError {
    match sqlx::query_scalar::<_, String>(
        "SELECT status FROM community_comments WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(reply_id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(status)) => CommunityReplyError::NotAwaiting(status),
        Ok(None) => CommunityReplyError::NotFound,
        Err(error) => CommunityReplyError::Database(error),
    }
}
