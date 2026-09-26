//! The account's Reddit standing, read from its own post history.
//!
//! `crowdrelay_domain::reddit_standing` decides; this module feeds it. Three
//! places consult it: the claim (how many a day), the send path (halted, or a
//! community that removed us, holds the draft for a person) and the metrics
//! read, which is where a removal is first seen and recorded.

use super::*;
use crowdrelay_domain::reddit_standing::{
    COMMUNITY_REMOVED_US, PostRecord, RedditStanding, RemovalCause, community_removed_us,
    reddit_standing,
};

/// How far back the standing reads. Covers the longest window it uses — a
/// community that removed us is remembered for 180 days.
const HISTORY_DAYS: i32 = 180;

/// Mirrors the SQL `normalize_subreddit`: lowercased, trimmed, `r/` stripped.
pub(super) fn normalized_subreddit(raw: &str) -> String {
    let lowered = raw.trim().to_lowercase();
    let without_slash = lowered.strip_prefix('/').unwrap_or(&lowered);
    without_slash
        .strip_prefix("r/")
        .unwrap_or(without_slash)
        .to_owned()
}

type HistoryRow = (
    String,
    OffsetDateTime,
    Option<String>,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
);

/// Every post this workspace published in the history window.
///
/// Campaign deliveries are included: they go out under the same account, and a
/// moderator removing one is the same verdict as removing any other.
pub(super) async fn post_history<'e, E>(
    executor: E,
    workspace_id: Uuid,
) -> Result<Vec<PostRecord>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows: Vec<HistoryRow> = sqlx::query_as(
        r#"
        SELECT normalize_subreddit(subreddit), posted_at, removed_by_category,
               removal_seen_at, last_seen_live_at
        FROM community_posts
        WHERE workspace_id = $1
          AND status = 'posted'
          AND posted_at IS NOT NULL
          AND posted_at > now() - make_interval(days => $2)
        "#,
    )
    .bind(workspace_id)
    .bind(HISTORY_DAYS)
    .fetch_all(executor)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(subreddit, posted_at, category, removal_seen_at, last_seen_live_at)| PostRecord {
                subreddit,
                posted_at,
                removal: category.as_deref().and_then(RemovalCause::from_category),
                removal_seen_at,
                last_seen_live_at,
            },
        )
        .collect())
}

/// How many uncampaigned posts the account may publish in 24 hours.
///
/// A halted account keeps the floor here rather than zero: the claim still
/// picks drafts up so the send path can hold each one with the reason, instead
/// of leaving them pending with nothing saying why.
pub(super) fn daily_cap(history: &[PostRecord]) -> i64 {
    match reddit_standing(history, OffsetDateTime::now_utc()) {
        RedditStanding::Open { daily_cap } => i64::from(daily_cap),
        RedditStanding::Halted(_) => i64::from(crowdrelay_domain::reddit_standing::BASE_DAILY_CAP),
    }
}

impl CommunityExecutorWorker {
    /// Why this draft must go to a person instead of Reddit, if it must.
    pub(super) async fn standing_hold(
        &self,
        subreddit: &str,
    ) -> Result<Option<&'static str>, CommunityExecutorError> {
        let history = post_history(&self.pool, self.workspace_id.into_uuid()).await?;
        let now = OffsetDateTime::now_utc();
        if let RedditStanding::Halted(reason) = reddit_standing(&history, now) {
            return Ok(Some(reason.as_str()));
        }
        if community_removed_us(&history, &normalized_subreddit(subreddit), now) {
            return Ok(Some(COMMUNITY_REMOVED_US));
        }
        Ok(None)
    }

    /// Whether the workspace has reached its earned 24h post limit.
    pub(super) async fn rate_limit_reached(&self) -> Result<bool, CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let history = post_history(&self.pool, ws).await?;
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM community_posts
            WHERE workspace_id = $1
              AND status = 'posted'
              AND posted_at > now() - INTERVAL '24 hours'
            "#,
        )
        .bind(ws)
        .fetch_one(&self.pool)
        .await?;
        Ok(count >= daily_cap(&history))
    }
}

/// Records what a removal-aware metrics read said about a post.
///
/// A read that could not see removal state (`removal_visible` false — an
/// agents service that predates the field) records nothing: it neither proves
/// the post live nor removed. A removal is recorded once, the first time it is
/// seen; a later read cannot un-remove it.
pub(super) async fn record_removal_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    post_id: Uuid,
    metrics: &RedditPostMetrics,
) -> Result<(), sqlx::Error> {
    if !metrics.removal_visible {
        return Ok(());
    }
    let category = metrics
        .removed_by_category
        .as_deref()
        .map(str::trim)
        .filter(|category| !category.is_empty())
        .map(|category| category.chars().take(64).collect::<String>());
    if let Some(category) = &category
        && RemovalCause::from_category(category).is_some_and(RemovalCause::is_verdict)
    {
        tracing::warn!(
            %post_id,
            removed_by_category = %category,
            "a published community post was removed — the account's standing is re-read before the next post"
        );
    }
    sqlx::query(
        r#"
        UPDATE community_posts
        SET removed_by_category = COALESCE(removed_by_category, $3),
            removal_seen_at = CASE
                WHEN removed_by_category IS NULL AND $3::text IS NOT NULL THEN now()
                ELSE removal_seen_at
            END,
            last_seen_live_at = CASE
                WHEN removed_by_category IS NULL AND $3::text IS NULL THEN now()
                ELSE last_seen_live_at
            END
        WHERE id = $1 AND workspace_id = $2
        "#,
    )
    .bind(post_id)
    .bind(workspace_id)
    .bind(category)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// `Some` whenever the key is present, `null` included — plain `Option`
/// would turn a present `null` into `None` and lose the difference.
pub(super) fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod standing_tests {
    use super::*;

    #[test]
    fn subreddit_names_normalise_like_the_sql_function() {
        for raw in ["r/Metal", "/r/metal", " Metal ", "metal"] {
            assert_eq!(normalized_subreddit(raw), "metal", "{raw}");
        }
    }

    #[test]
    fn a_listing_without_the_removal_key_proves_nothing() {
        let live: RedditPostData = serde_json::from_value(serde_json::json!({
            "score": 3, "ups": 3, "num_comments": 0, "upvote_ratio": 1.0,
            "removed_by_category": null
        }))
        .expect("live post");
        assert!(matches!(live.removed_by_category, Some(Value::Null)));
        let silent: RedditPostData = serde_json::from_value(serde_json::json!({
            "score": 3, "ups": 3, "num_comments": 0, "upvote_ratio": 1.0
        }))
        .expect("listing without the key");
        assert!(silent.removed_by_category.is_none());
        let removed: RedditPostData = serde_json::from_value(serde_json::json!({
            "score": 1, "ups": 1, "num_comments": 0, "upvote_ratio": 1.0,
            "removed_by_category": "moderator"
        }))
        .expect("removed post");
        assert_eq!(
            removed.removed_by_category.as_ref().and_then(Value::as_str),
            Some("moderator")
        );
    }

    #[test]
    fn a_fresh_account_is_capped_at_one_post_a_day() {
        assert_eq!(daily_cap(&[]), 1);
    }
}
