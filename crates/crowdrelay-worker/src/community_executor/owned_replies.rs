//! The reply lane on the band's own channels: comments under its Instagram
//! and Facebook posts.
//!
//! Same queue, drafter, guards, review and routing as Reddit
//! (`replies.rs`); what differs is the transport. Comments are read and
//! answered through the Graph API with the Page token the post sync already
//! holds, not through a browser session, and there is no community standing
//! to protect — the posts are the band's own.
//!
//! - **Harvest** only when the post sync saw a post's comment count grow past
//!   what was last harvested (`metadata.comments_count` vs
//!   `metadata.comments_harvested`): no reads for quiet posts.
//! - **Send** only with the owned-channel publish gate
//!   (`CROWDRELAY_SOCIAL_AUTO_POST`) on, one at a time, spaced and capped.

use super::*;
use crowdrelay_domain::community_reply::{
    HarvestedComment, OWNED_MAX_REPLIES_PER_24H, OWNED_MIN_REPLY_GAP, comments_to_answer,
};

const GRAPH_API_BASE: &str = "https://graph.facebook.com/v21.0";
/// Synced posts harvested per cycle.
const OWNED_POSTS_PER_CYCLE: i64 = 5;
/// How long after posting a post's comments are still harvested.
const OWNED_HARVEST_DAYS: i32 = 14;
const OWNED_HARVEST_PER_POST: usize = 25;
const GRAPH_TIMEOUT: Duration = Duration::from_secs(20);

fn flag(variable: &str) -> bool {
    std::env::var(variable)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Whether a clean, reviewed reply on the band's own channels may be approved
/// without a person: the owned-channel reply switch and the owned-channel
/// publish gate, both. Off unless set.
pub(super) fn unattended_owned_replies_enabled() -> bool {
    flag("CROWDRELAY_OWNED_AUTO_REPLY") && flag("CROWDRELAY_SOCIAL_AUTO_POST")
}

/// A Graph id: digits, and underscores in Facebook's `page_post` shape.
fn is_graph_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_digit() || c == '_')
}

#[derive(Deserialize)]
struct GraphPage<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

#[derive(Deserialize)]
struct GraphFrom {
    id: Option<String>,
    username: Option<String>,
    name: Option<String>,
}

/// An Instagram or Facebook comment, with one level of replies — the shape
/// both platforms answer with for the fields asked below.
#[derive(Deserialize)]
struct GraphComment {
    id: String,
    /// Instagram's text field.
    text: Option<String>,
    /// Facebook's text field.
    message: Option<String>,
    from: Option<GraphFrom>,
    /// Instagram's nested replies.
    replies: Option<GraphPage<GraphComment>>,
    /// Facebook's nested replies.
    comments: Option<GraphPage<GraphComment>>,
}

#[derive(Deserialize)]
struct GraphCreated {
    id: String,
}

#[derive(sqlx::FromRow)]
struct OwnedSource {
    id: Uuid,
    source_key: String,
    platform: String,
    comments_count: i64,
}

/// Flattens a post's comments and their replies into what the domain reads.
/// `account_id` is the band's own Page / Instagram account.
fn flatten(post_id: &str, account_id: &str, comments: Vec<GraphComment>) -> Vec<HarvestedComment> {
    let mut out = Vec::new();
    let mut push = |comment: &GraphComment, parent: &str| {
        let from = comment.from.as_ref();
        let author = from
            .and_then(|f| f.username.clone().or_else(|| f.name.clone()))
            .unwrap_or_else(|| "someone".to_owned());
        out.push(HarvestedComment {
            id: comment.id.clone(),
            parent_id: parent.to_owned(),
            author,
            body: comment
                .text
                .clone()
                .or_else(|| comment.message.clone())
                .unwrap_or_default(),
            by_band: from.and_then(|f| f.id.as_deref()) == Some(account_id),
            gone: false,
        });
    };
    for comment in &comments {
        push(comment, post_id);
        let nested = comment.replies.as_ref().or(comment.comments.as_ref());
        for reply in nested.map(|page| page.data.as_slice()).unwrap_or_default() {
            push(reply, &comment.id);
        }
    }
    out
}

impl CommunityExecutorWorker {
    /// Harvests comments on the band's own posts whose count grew.
    pub(super) async fn harvest_owned_comments(&self) -> Result<usize, CommunityExecutorError> {
        let Some(token) = self.facebook_page_access_token.as_deref() else {
            return Ok(0);
        };
        let ws = self.workspace_id.into_uuid();
        let sources: Vec<OwnedSource> = sqlx::query_as(
            r#"
            SELECT id, source_key, metadata->>'platform' AS platform,
                   (metadata->>'comments_count')::bigint AS comments_count
            FROM content_sources
            WHERE workspace_id = $1
              AND source_kind = 'social_post'
              AND active
              AND metadata->>'platform' IN ('instagram', 'facebook')
              AND jsonb_typeof(metadata->'comments_count') = 'number'
              AND (metadata->>'comments_count')::bigint
                  > COALESCE((metadata->>'comments_harvested')::bigint, 0)
              AND occurred_at > now() - make_interval(days => $2)
            ORDER BY occurred_at DESC
            LIMIT $3
            "#,
        )
        .bind(ws)
        .bind(OWNED_HARVEST_DAYS)
        .bind(OWNED_POSTS_PER_CYCLE)
        .fetch_all(&self.pool)
        .await?;
        let mut harvested = 0;
        for source in sources {
            let Some(post_id) = source
                .source_key
                .split_once(':')
                .map(|(_, id)| id.to_owned())
                .filter(|id| is_graph_id(id))
            else {
                continue;
            };
            let account_id: Option<String> = sqlx::query_scalar(
                r#"
                SELECT provider_account_id FROM fanbase_connections
                WHERE workspace_id = $1 AND platform = $2 AND status = 'connected'
                  AND provider_account_id IS NOT NULL
                LIMIT 1
                "#,
            )
            .bind(ws)
            .bind(&source.platform)
            .fetch_optional(&self.pool)
            .await?;
            let Some(account_id) = account_id else {
                continue;
            };
            let fields = if source.platform == "instagram" {
                "id,text,from{id,username},replies{id,text,from{id,username}}"
            } else {
                "id,message,from{id,name},comments{id,message,from{id,name}}"
            };
            let url = format!("{GRAPH_API_BASE}/{post_id}/comments");
            let client = self
                .http_client
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let response = client
                .get(&url)
                .query(&[("fields", fields), ("limit", "50"), ("access_token", token)])
                .timeout(GRAPH_TIMEOUT)
                .send()
                .await
                .map_err(|error| CommunityExecutorError::Http(error.without_url()))?;
            if !response.status().is_success() {
                // A missing permission (instagram_manage_comments,
                // pages_read_user_content) answers 4xx here — named once per
                // cycle, never retried in a loop.
                tracing::warn!(
                    platform = %source.platform,
                    status = %response.status(),
                    "owned-channel comments read refused"
                );
                continue;
            }
            check_response_size(&response)?;
            let page: GraphPage<GraphComment> = response.json().await?;
            let comments = flatten(&post_id, &account_id, page.data);
            let mut tx = self.pool.begin().await?;
            for comment in comments_to_answer(&post_id, &comments)
                .into_iter()
                .filter(|c| is_graph_id(&c.id) && is_graph_id(&c.parent_id))
                .take(OWNED_HARVEST_PER_POST)
            {
                let parent = comments.iter().find(|c| c.id == comment.parent_id);
                let inserted = sqlx::query(
                    r#"
                    INSERT INTO community_comments
                        (workspace_id, platform, content_source_id, platform_comment_id,
                         parent_id, author, body, parent_body, parent_by_band)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                    ON CONFLICT (workspace_id, platform, platform_comment_id) DO NOTHING
                    "#,
                )
                .bind(ws)
                .bind(&source.platform)
                .bind(source.id)
                .bind(&comment.id)
                .bind(&comment.parent_id)
                .bind(comment.author.chars().take(64).collect::<String>())
                .bind(comment.body.chars().take(4000).collect::<String>())
                .bind(parent.map(|p| p.body.chars().take(4000).collect::<String>()))
                .bind(parent.is_some_and(|p| p.by_band))
                .execute(&mut *tx)
                .await?;
                harvested += usize::try_from(inserted.rows_affected()).unwrap_or(0);
            }
            sqlx::query(
                r#"
                UPDATE content_sources
                SET metadata = metadata || jsonb_build_object('comments_harvested', $3::bigint)
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(ws)
            .bind(source.id)
            .bind(source.comments_count)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        Ok(harvested)
    }

    /// Sends one due reply on the band's own channels through the Graph API.
    pub(super) async fn send_due_owned_reply(&self) -> Result<usize, CommunityExecutorError> {
        let Some(token) = self.facebook_page_access_token.clone() else {
            return Ok(0);
        };
        if !flag("CROWDRELAY_SOCIAL_AUTO_POST") {
            return Ok(0);
        }
        let ws = self.workspace_id.into_uuid();
        let (sent_24h, last_sent): (i64, Option<OffsetDateTime>) = sqlx::query_as(
            r#"
            SELECT count(*) FILTER (WHERE replied_at > now() - INTERVAL '24 hours'),
                   max(replied_at)
            FROM community_comments
            WHERE workspace_id = $1 AND status = 'replied' AND platform <> 'reddit'
            "#,
        )
        .bind(ws)
        .fetch_one(&self.pool)
        .await?;
        if sent_24h >= OWNED_MAX_REPLIES_PER_24H
            || last_sent.is_some_and(|at| OffsetDateTime::now_utc() - at < OWNED_MIN_REPLY_GAP)
        {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let row: Option<(Uuid, String, String, String, i32)> = sqlx::query_as(
            r#"
            SELECT id, platform, platform_comment_id, draft, attempts
            FROM community_comments
            WHERE workspace_id = $1 AND status = 'approved'
              AND platform IN ('instagram', 'facebook')
              AND (not_before IS NULL OR not_before <= now())
            ORDER BY not_before NULLS FIRST, created_at
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, platform, comment_id, draft, attempts)) = row else {
            return Ok(0);
        };
        sqlx::query(
            "UPDATE community_comments SET status = 'replying', attempts = attempts + 1, updated_at = now() WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id)
        .bind(ws)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        // Instagram answers a comment at /replies, Facebook at /comments.
        let edge = if platform == "instagram" {
            "replies"
        } else {
            "comments"
        };
        let client = self
            .http_client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let result = client
            .post(format!("{GRAPH_API_BASE}/{comment_id}/{edge}"))
            .form(&[
                ("message", draft.as_str()),
                ("access_token", token.as_str()),
            ])
            .timeout(GRAPH_TIMEOUT)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                let created: GraphCreated = response.json().await?;
                if !is_graph_id(&created.id) {
                    return Err(CommunityExecutorError::RedditApi(
                        "graph returned a reply id that is not a Graph id".to_owned(),
                    ));
                }
                sqlx::query(
                    r#"
                    UPDATE community_comments
                    SET status = 'replied', reply_comment_id = $3, replied_at = now(),
                        hold_reason = NULL, updated_at = now()
                    WHERE id = $1 AND workspace_id = $2
                    "#,
                )
                .bind(id)
                .bind(ws)
                .bind(&created.id)
                .execute(&self.pool)
                .await?;
                Ok(1)
            }
            Ok(response) if response.status().is_client_error() => {
                // The platform refused this reply (a deleted comment, a
                // missing permission) — terminal for it, with the reason.
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                sqlx::query(
                    "UPDATE community_comments SET status = 'failed', hold_reason = $3, updated_at = now() WHERE id = $1 AND workspace_id = $2",
                )
                .bind(id)
                .bind(ws)
                .bind(
                    format!("{platform} refused the reply (HTTP {status}): {body}")
                        .chars()
                        .take(500)
                        .collect::<String>(),
                )
                .execute(&self.pool)
                .await?;
                Ok(0)
            }
            Ok(response) => {
                tracing::warn!(status = %response.status(), "owned reply send deferred");
                self.back_off(id, attempts + 1, "approved").await?;
                Ok(0)
            }
            Err(error) => {
                tracing::warn!(error = %error.without_url(), "owned reply send deferred");
                self.back_off(id, attempts + 1, "approved").await?;
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
mod owned_replies_tests {
    use super::*;

    #[test]
    fn instagram_threads_flatten_with_the_band_marked() {
        let comments: Vec<GraphComment> = serde_json::from_value(serde_json::json!([
            {
                "id": "111",
                "text": "kiedy następny koncert?",
                "from": { "id": "900", "username": "fan1" },
                "replies": { "data": [
                    { "id": "112", "text": "17.10 Gorzów!", "from": { "id": "777", "username": "virya" } }
                ] }
            },
            { "id": "113", "text": "🔥", "from": { "id": "901", "username": "fan2" } },
            { "id": "114", "text": "Czy będzie winyl?", "from": { "id": "902", "username": "fan3" } }
        ]))
        .expect("graph comments");
        let flat = flatten("555", "777", comments);
        assert_eq!(flat.len(), 4);
        assert_eq!(flat[1].parent_id, "111");
        assert!(flat[1].by_band);
        // 111 is answered, 113 has no words: only the vinyl question waits.
        let answer: Vec<&str> = comments_to_answer("555", &flat)
            .into_iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(answer, vec!["114"]);
    }

    #[test]
    fn facebook_comments_read_message_and_nested_comments() {
        let comments: Vec<GraphComment> = serde_json::from_value(serde_json::json!([
            {
                "id": "5_1",
                "message": "What tuning is this?",
                "comments": { "data": [
                    { "id": "5_2", "message": "Drop C.", "from": { "id": "42", "name": "Virya" } },
                    { "id": "5_3", "message": "Thanks!", "from": { "id": "8", "name": "Fan" } }
                ] }
            }
        ]))
        .expect("graph comments");
        let flat = flatten("42_5", "42", comments);
        assert_eq!(flat[0].author, "someone", "facebook may withhold `from`");
        let answer: Vec<&str> = comments_to_answer("42_5", &flat)
            .into_iter()
            .map(|c| c.id.as_str())
            .collect();
        // 5_1 was answered by the band; 5_3 replies to a fan, not the band.
        assert!(answer.is_empty(), "{answer:?}");
    }

    #[test]
    fn only_graph_ids_pass() {
        assert!(is_graph_id("17912345678901234"));
        assert!(is_graph_id("123_456"));
        assert!(!is_graph_id("t1_abc"));
        assert!(!is_graph_id("../me"));
        assert!(!is_graph_id(""));
    }

    #[test]
    fn owned_replies_stay_attended_unless_switched_on() {
        assert!(!unattended_owned_replies_enabled());
    }
}
