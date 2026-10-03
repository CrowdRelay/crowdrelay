//! The account's Reddit standing, read from its own post history.
//!
//! `crowdrelay_domain::reddit_standing` decides; this module feeds it. Three
//! places consult it: the claim (how many a day), the send path (halted, or a
//! community that removed us, holds the draft for a person) and the metrics
//! read, which is where a removal is first seen and recorded.

use super::*;
use crowdrelay_domain::community_register::review_community_register;
use crowdrelay_domain::community_reply::ReviewOutcome;
use crowdrelay_domain::posting_window::{
    default_active_hours, learned_active_hours, wait_before_posting,
};
use crowdrelay_domain::reddit_standing::{
    COMMUNITY_REMOVED_US, PostRecord, RedditStanding, RemovalCause, autonomy_hold_reason,
    community_removed_us, reddit_standing,
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
    bool,
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
        SELECT normalize_subreddit(post.subreddit), post.posted_at,
               post.removed_by_category, post.removal_seen_at, post.last_seen_live_at,
               (
                   COALESCE(latest.score > 1, false)
                   OR EXISTS (
                       SELECT 1
                       FROM community_comments AS human_comment
                       WHERE human_comment.workspace_id = post.workspace_id
                         AND human_comment.community_post_id = post.id
                   )
               )
        FROM community_posts AS post
        LEFT JOIN LATERAL (
            SELECT metric.score, metric.num_comments
            FROM community_post_metrics AS metric
            WHERE metric.workspace_id = post.workspace_id
              AND metric.community_post_id = post.id
            ORDER BY metric.measured_at DESC, metric.id DESC
            LIMIT 1
        ) AS latest ON true
        WHERE post.workspace_id = $1
          AND post.status = 'posted'
          AND post.posted_at IS NOT NULL
          AND post.posted_at > now() - make_interval(days => $2)
        "#,
    )
    .bind(workspace_id)
    .bind(HISTORY_DAYS)
    .fetch_all(executor)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                subreddit,
                posted_at,
                category,
                removal_seen_at,
                last_seen_live_at,
                community_responded,
            )| PostRecord {
                subreddit,
                posted_at,
                removal: category.as_deref().and_then(RemovalCause::from_category),
                removal_seen_at,
                last_seen_live_at,
                community_responded,
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
    /// Why this draft must go to a person instead of Reddit, if it must: the
    /// account is halted, this community removed us, or the draft reads like
    /// marketing or a repeat (`community_register`).
    pub(super) async fn standing_hold(
        &self,
        action: &ClaimedAction,
    ) -> Result<Option<String>, CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let history = post_history(&self.pool, ws).await?;
        let now = OffsetDateTime::now_utc();
        if let RedditStanding::Halted(reason) = reddit_standing(&history, now) {
            return Ok(Some(reason.as_str().to_owned()));
        }
        if let Some(reason) = autonomy_hold_reason(&history, now) {
            return Ok(Some(reason.as_str().to_owned()));
        }
        if community_removed_us(&history, &normalized_subreddit(&action.subreddit), now) {
            return Ok(Some(COMMUNITY_REMOVED_US.to_owned()));
        }
        // Title and drafted body, as a reader sees them — before the tracked
        // link is appended, which carries a per-action slug and would make
        // every post look new.
        let recent: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT title || E'\n' || body FROM community_posts
            WHERE workspace_id = $1
              AND status = 'posted'
              AND posted_at > now() - INTERVAL '30 days'
            "#,
        )
        .bind(ws)
        .fetch_all(&self.pool)
        .await?;
        let draft = format!("{}\n{}", action.title, action.body);
        if let Some(hold) = review_community_register(&draft, &recent) {
            return Ok(Some(hold.as_str().to_owned()));
        }
        // Last, because it is the only check that spends a model call: the
        // independent review. A failed review, or none at all, is a person.
        let link = action
            .source_url
            .as_deref()
            .or(action.smart_link.as_deref())
            .unwrap_or("(no link)");
        let context = format!(
            "The post shares the band's own content, linked here: {link}\n\
             Introducing or naming what that link carries is grounded; any \
             other factual claim needs support in the post itself."
        );
        Ok(
            match self
                .review_text("reddit_post", &action.subreddit, &draft, &context, None)
                .await
            {
                ReviewOutcome::Passed { .. } => None,
                ReviewOutcome::Failed { score } => Some(format!(
                    "held: the independent review scored it {score}/10 — rewrite before it goes out"
                )),
                ReviewOutcome::Unavailable => Some(
                    "held: no reviewer could be reached — read it before it goes out".to_owned(),
                ),
            },
        )
    }

    /// Defers the post until its community is awake and it has settled
    /// (`crowdrelay_domain::posting_window`). Returns whether it deferred.
    ///
    /// A deferral is not an attempt: the claim's attempt bump is refunded, or
    /// a post waiting overnight for its window would spend the transient
    /// failure budget it never failed against.
    pub(super) async fn defer_to_posting_window(
        &self,
        action: &ClaimedAction,
    ) -> Result<bool, CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let (language, ready_at): (Option<String>, OffsetDateTime) = sqlx::query_as(
            r#"
            SELECT target.language, post.created_at
            FROM community_posts AS post
            LEFT JOIN agent_outreach_targets AS target
              ON target.id = post.target_id AND target.workspace_id = post.workspace_id
            WHERE post.id = $1 AND post.workspace_id = $2
            "#,
        )
        .bind(action.id)
        .bind(ws)
        .fetch_one(&self.pool)
        .await?;
        // The community's own online-user counts by UTC hour, last 30 days.
        let samples: Vec<(i32, i64)> = sqlx::query_as(
            r#"
            SELECT EXTRACT(HOUR FROM obs.observed_at AT TIME ZONE 'UTC')::int,
                   (obs.raw_activity_metrics->>'online_users')::bigint
            FROM community_observations AS obs
            JOIN agent_outreach_targets AS target
              ON target.place_id = obs.place_id AND target.workspace_id = obs.workspace_id
            WHERE obs.workspace_id = $1
              AND target.id = $2
              AND jsonb_typeof(obs.raw_activity_metrics->'online_users') = 'number'
              AND obs.observed_at > now() - INTERVAL '30 days'
            "#,
        )
        .bind(ws)
        .bind(action.target_id)
        .fetch_all(&self.pool)
        .await?;
        let samples: Vec<(u8, i64)> = samples
            .into_iter()
            .filter_map(|(hour, online)| u8::try_from(hour).ok().map(|h| (h, online)))
            .collect();
        let active = learned_active_hours(&samples)
            .unwrap_or_else(|| default_active_hours(language.as_deref()));
        let Some(wait) = wait_before_posting(
            OffsetDateTime::now_utc(),
            ready_at,
            &active,
            action.id.as_u128(),
        ) else {
            return Ok(false);
        };
        let wait = Duration::try_from(wait).unwrap_or(Duration::from_secs(600));
        tracing::info!(
            post_id = %action.id,
            subreddit = %action.subreddit,
            wait_minutes = wait.as_secs() / 60,
            "deferring to the community's active hours"
        );
        self.mark_rate_limited(action.id, wait).await?;
        sqlx::query(
            "UPDATE community_posts SET attempts = GREATEST(attempts - 1, 0) WHERE id = $1 AND workspace_id = $2",
        )
        .bind(action.id)
        .bind(ws)
        .execute(&self.pool)
        .await?;
        Ok(true)
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
    .bind(&category)
    .execute(&mut **tx)
    .await?;

    // A verdict removal is the community's own answer about us, in writing.
    // Feeding it back closes the loop the standing halt can only half-cover:
    // the halt stops the account; this stops the target. Without it a
    // community that removed us stays `admitted`, the brain drafts it again
    // when the halt lifts, and the second removal lands on a moderator who
    // already said no once.
    if let Some(category) = &category
        && RemovalCause::from_category(category).is_some_and(RemovalCause::is_verdict)
    {
        sqlx::query(
            r#"
            UPDATE agent_outreach_targets
            SET screening_verdict = 'refused',
                refusal_reason = 'previously_refused',
                status = 'discarded',
                updated_at = now()
            WHERE workspace_id = $1
              AND id = (SELECT target_id FROM community_posts
                        WHERE id = $2 AND workspace_id = $1)
              AND screening_verdict = 'admitted'
            "#,
        )
        .bind(workspace_id)
        .bind(post_id)
        .execute(&mut **tx)
        .await?;
        // The place the target points at is marked too — the discovery side
        // of the same door, so a future screen does not re-walk it as a
        // fresh candidate.
        sqlx::query(
            r#"
            UPDATE discovery_places
            SET membership_state = 'not_a_fit', updated_at = now()
            WHERE workspace_id = $1
              AND id = (SELECT place_id FROM community_posts
                        WHERE id = $2 AND workspace_id = $1)
              AND membership_state IS DISTINCT FROM 'not_a_fit'
            "#,
        )
        .bind(workspace_id)
        .bind(post_id)
        .execute(&mut **tx)
        .await?;
    }
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
