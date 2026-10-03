//! The reply lane on the band's YouTube videos.
//!
//! Same queue as Reddit, Instagram and Facebook (`community_comments`): the
//! community executor's reply lane drafts, reviews and routes every row
//! whatever its platform. This worker is the YouTube transport only:
//!
//! - **Harvest** comments under the band's own uploads (`content_sources`
//!   rows the video sync filed as `youtube:{id}`) with the API key the video
//!   sync already holds — public data, no grant. Recent videos only, each
//!   re-read at most every six hours, so quota is spent where people talk.
//! - **Send** an approved reply with the channel owner's grant (connection
//!   platform `youtube_account`, scope `youtube.force-ssl`), under the same
//!   owned-channel publish gate, cap and spacing as Instagram and Facebook.
//! - **Capture** a fan from a fresh upload with one first-party comment carrying
//!   the tenant's own join-ask words and a source-attributed `/signal` link.
//!   Videos whose description already has a Signal CTA are left alone.
//!
//! Without an API key the worker never starts; without a grant it harvests
//! and drafts, and approved replies wait with a reason instead of failing.

use std::time::Duration;

use crowdrelay_domain::community_reply::{
    HarvestedComment, OWNED_MAX_REPLIES_PER_24H, OWNED_MIN_REPLY_GAP, comments_to_answer,
};
use crowdrelay_infra::{
    gdrive::PostgresGDriveRepository, sensitive_response::SensitiveResponseKey,
};
use serde::Deserialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::sync::watch;
use uuid::Uuid;

use crate::google_oauth::resolve_google_access_token;

mod fan_capture;
pub mod fan_capture_draft;

const API_BASE: &str = "https://www.googleapis.com/youtube/v3";
const CYCLE: Duration = Duration::from_secs(30 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Uploads whose comments are still read.
const HARVEST_DAYS: i32 = 30;
const VIDEOS_PER_CYCLE: i64 = 5;
const ANSWERS_PER_VIDEO: usize = 25;
/// Attempts before a transient send failure becomes terminal.
const MAX_ATTEMPTS: i32 = 5;
const RETRY_BACKOFF_MINUTES: i32 = 30;

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

/// A YouTube id: URL-safe characters, and a `.` in a reply's
/// `{parent}.{suffix}` form. Anything else never reaches a URL or the table.
fn is_youtube_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The thread a reply must be posted under. YouTube threads are one level
/// deep: a reply to a reply goes under the top-level comment, whose id is
/// the part of the reply's id before the dot.
fn thread_parent(comment_id: &str) -> &str {
    comment_id.split('.').next().unwrap_or(comment_id)
}

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
}

#[derive(Deserialize)]
struct ChannelRef {
    value: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentSnippet {
    author_display_name: Option<String>,
    author_channel_id: Option<ChannelRef>,
    text_original: Option<String>,
    text_display: Option<String>,
    parent_id: Option<String>,
}

#[derive(Deserialize)]
struct Comment {
    id: String,
    snippet: CommentSnippet,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadSnippet {
    top_level_comment: Comment,
}

#[derive(Deserialize)]
struct Replies {
    #[serde(default = "Vec::new")]
    comments: Vec<Comment>,
}

#[derive(Deserialize)]
struct Thread {
    snippet: ThreadSnippet,
    replies: Option<Replies>,
}

#[derive(Deserialize)]
struct Created {
    id: String,
}

/// Flattens a video's comment threads into what the domain reads. Top-level
/// comments hang off the video id; replies off their thread.
fn flatten(
    video_id: &str,
    channel_id: Option<&str>,
    threads: Vec<Thread>,
) -> Vec<HarvestedComment> {
    let mut out = Vec::new();
    let mut push = |comment: &Comment, parent: &str| {
        let snippet = &comment.snippet;
        out.push(HarvestedComment {
            id: comment.id.clone(),
            parent_id: parent.to_owned(),
            author: snippet
                .author_display_name
                .clone()
                .unwrap_or_else(|| "someone".to_owned()),
            body: snippet
                .text_original
                .clone()
                .or_else(|| snippet.text_display.clone())
                .unwrap_or_default(),
            by_band: channel_id.is_some()
                && snippet
                    .author_channel_id
                    .as_ref()
                    .and_then(|author| author.value.as_deref())
                    == channel_id,
            gone: false,
        });
    };
    for thread in &threads {
        let top = &thread.snippet.top_level_comment;
        push(top, video_id);
        for reply in thread
            .replies
            .as_ref()
            .map(|r| r.comments.as_slice())
            .unwrap_or_default()
        {
            let parent = reply
                .snippet
                .parent_id
                .as_deref()
                .unwrap_or(top.id.as_str());
            push(reply, parent);
        }
    }
    out
}

pub struct YoutubeRepliesWorker {
    pool: PgPool,
    workspace_id: Uuid,
    http: reqwest::Client,
    api_key: String,
    repo: PostgresGDriveRepository,
    key: SensitiveResponseKey,
    google_client_id: Option<String>,
    google_client_secret: Option<String>,
}

impl YoutubeRepliesWorker {
    /// Spawns the worker when `CROWDRELAY_YOUTUBE_API_KEY` is set. One call
    /// site in `main.rs`, which is at its size ratchet.
    pub fn spawn_if_configured(
        tasks: &mut tokio::task::JoinSet<&'static str>,
        pool: PgPool,
        workspace_id: Uuid,
        key: SensitiveResponseKey,
        shutdown: watch::Receiver<bool>,
    ) -> bool {
        let Some(api_key) = std::env::var("CROWDRELAY_YOUTUBE_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
        else {
            return false;
        };
        let Ok(http) = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(HTTP_TIMEOUT)
            .user_agent("CrowdRelay/1.0 (youtube replies)")
            .build()
        else {
            tracing::warn!("youtube replies: http client could not be built; worker not started");
            return false;
        };
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let worker = Self {
            repo: PostgresGDriveRepository::new(pool.clone()),
            pool,
            workspace_id,
            http,
            api_key,
            key,
            google_client_id: env("CROWDRELAY_GOOGLE_ADS_CLIENT_ID"),
            google_client_secret: env("CROWDRELAY_GOOGLE_ADS_CLIENT_SECRET"),
        };
        tasks.spawn(async move {
            worker.run(shutdown).await;
            "youtube replies"
        });
        true
    }

    async fn run(self, mut shutdown: watch::Receiver<bool>) {
        tracing::info!("youtube replies worker started");
        let mut tick = tokio::time::interval(CYCLE);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = tick.tick() => {
                    match self.harvest().await {
                        Ok(count) if count > 0 => tracing::info!(count, "youtube comments harvested"),
                        Ok(_) => {}
                        Err(error) => tracing::warn!(%error, "youtube comment harvest failed"),
                    }
                    match self.seed_fan_capture_comment().await {
                        Ok(count) if count > 0 => tracing::info!(count, "youtube fan-capture comment posted or prepared for a person"),
                        Ok(_) => {}
                        Err(error) => tracing::warn!(%error, "youtube fan-capture comment failed"),
                    }
                    if let Err(error) = self.send_due().await {
                        tracing::warn!(%error, "youtube reply send failed");
                    }
                }
            }
        }
    }

    async fn harvest(&self) -> Result<usize, sqlx::Error> {
        let videos: Vec<(Uuid, String)> = sqlx::query_as(
            r#"
            SELECT id, source_key FROM content_sources
            WHERE workspace_id = $1
              AND source_kind = 'video'
              AND source_key LIKE 'youtube:%'
              AND active
              AND occurred_at > now() - make_interval(days => $2)
              AND (NOT (metadata ? 'comments_harvested_at')
                   OR (metadata->>'comments_harvested_at')::timestamptz < now() - interval '6 hours')
            ORDER BY occurred_at DESC
            LIMIT $3
            "#,
        )
        .bind(self.workspace_id)
        .bind(HARVEST_DAYS)
        .bind(VIDEOS_PER_CYCLE)
        .fetch_all(&self.pool)
        .await?;
        if videos.is_empty() {
            return Ok(0);
        }
        let channel_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT provider_account_id FROM fanbase_connections
            WHERE workspace_id = $1 AND platform = 'youtube' AND status = 'connected'
              AND provider_account_id IS NOT NULL
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id)
        .fetch_optional(&self.pool)
        .await?;
        let mut harvested = 0;
        for (source_id, source_key) in videos {
            let Some(video_id) = source_key
                .strip_prefix("youtube:")
                .filter(|id| is_youtube_id(id))
            else {
                continue;
            };
            let response = self
                .http
                .get(format!("{API_BASE}/commentThreads"))
                .query(&[
                    ("part", "snippet,replies"),
                    ("videoId", video_id),
                    ("maxResults", "50"),
                    ("order", "time"),
                    ("textFormat", "plainText"),
                    ("key", self.api_key.as_str()),
                ])
                .send()
                .await;
            let threads: Vec<Thread> = match response {
                Ok(response) if response.status().is_success() => {
                    match response.json::<Page<Thread>>().await {
                        Ok(page) => page.items,
                        Err(error) => {
                            tracing::warn!(error = %error.without_url(), "youtube comments unreadable");
                            continue;
                        }
                    }
                }
                // Comments disabled on a video answers 403; a quota wall the
                // same. Named, never retried in a loop — the six-hour
                // re-read spacing below still applies.
                Ok(response) => {
                    tracing::warn!(status = %response.status(), "youtube comments read refused");
                    Vec::new()
                }
                Err(error) => {
                    tracing::warn!(error = %error.without_url(), "youtube comments read failed");
                    continue;
                }
            };
            let comments = flatten(video_id, channel_id.as_deref(), threads);
            let mut tx = self.pool.begin().await?;
            for comment in comments_to_answer(video_id, &comments)
                .into_iter()
                .filter(|c| is_youtube_id(&c.id) && is_youtube_id(&c.parent_id))
                .take(ANSWERS_PER_VIDEO)
            {
                let parent = comments.iter().find(|c| c.id == comment.parent_id);
                let inserted = sqlx::query(
                    r#"
                    INSERT INTO community_comments
                        (workspace_id, platform, content_source_id, platform_comment_id,
                         parent_id, author, body, parent_body, parent_by_band)
                    VALUES ($1, 'youtube', $2, $3, $4, $5, $6, $7, $8)
                    ON CONFLICT (workspace_id, platform, platform_comment_id) DO NOTHING
                    "#,
                )
                .bind(self.workspace_id)
                .bind(source_id)
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
                SET metadata = metadata || jsonb_build_object('comments_harvested_at', now())
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(self.workspace_id)
            .bind(source_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        Ok(harvested)
    }

    /// The channel owner's grant, refreshed if due. `None` with no grant.
    async fn access_token(&self) -> Result<Option<String>, sqlx::Error> {
        let grant: Option<(Uuid, String)> = sqlx::query_as(
            r#"
            SELECT id, external_account_ref FROM fanbase_connections
            WHERE workspace_id = $1 AND platform = 'youtube_account' AND status = 'connected'
            ORDER BY updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((connection_id, account_ref)) = grant else {
            return Ok(None);
        };
        match resolve_google_access_token(
            &self.repo,
            &self.http,
            &self.key,
            self.workspace_id,
            connection_id,
            &account_ref,
            "youtube_account",
            self.google_client_id.as_deref(),
            self.google_client_secret.as_deref(),
        )
        .await
        {
            Ok(token) => Ok(Some(token)),
            Err(error) => {
                tracing::warn!(%error, "youtube grant could not produce an access token");
                Ok(None)
            }
        }
    }

    async fn send_due(&self) -> Result<usize, sqlx::Error> {
        if !flag("CROWDRELAY_SOCIAL_AUTO_POST") {
            return Ok(0);
        }
        if let Some(breaches) =
            crowdrelay_infra::scout_lane::halted(&self.pool, self.workspace_id).await
        {
            tracing::warn!(?breaches, "scout lane halted; youtube replies are held");
            return Ok(0);
        }
        // One cap and one spacing across the band's own channels.
        let (sent_24h, last_sent): (i64, Option<OffsetDateTime>) = sqlx::query_as(
            r#"
            SELECT count(*) FILTER (WHERE replied_at > now() - INTERVAL '24 hours'),
                   max(replied_at)
            FROM community_comments
            WHERE workspace_id = $1 AND status = 'replied' AND platform <> 'reddit'
            "#,
        )
        .bind(self.workspace_id)
        .fetch_one(&self.pool)
        .await?;
        if sent_24h >= OWNED_MAX_REPLIES_PER_24H
            || last_sent.is_some_and(|at| OffsetDateTime::now_utc() - at < OWNED_MIN_REPLY_GAP)
        {
            return Ok(0);
        }
        let Some(token) = self.access_token().await? else {
            // Approved replies wait for the grant rather than failing: say so
            // on the rows once, where the queue shows it.
            sqlx::query(
                r#"
                UPDATE community_comments
                SET hold_reason = 'waiting: connect YouTube (replies) so approved answers can be posted'
                WHERE workspace_id = $1 AND platform = 'youtube' AND status = 'approved'
                  AND hold_reason IS DISTINCT FROM
                      'waiting: connect YouTube (replies) so approved answers can be posted'
                "#,
            )
            .bind(self.workspace_id)
            .execute(&self.pool)
            .await?;
            return Ok(0);
        };
        let mut tx = self.pool.begin().await?;
        let row: Option<(Uuid, String, String, i32)> = sqlx::query_as(
            r#"
            SELECT id, platform_comment_id, draft, attempts
            FROM community_comments
            WHERE workspace_id = $1 AND status = 'approved' AND platform = 'youtube'
              AND draft IS NOT NULL
              AND (not_before IS NULL OR not_before <= now())
            ORDER BY not_before NULLS FIRST, created_at
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(self.workspace_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, comment_id, draft, attempts)) = row else {
            return Ok(0);
        };
        sqlx::query(
            "UPDATE community_comments SET status = 'replying', attempts = attempts + 1, updated_at = now() WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id)
        .bind(self.workspace_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let body = serde_json::json!({
            "snippet": { "parentId": thread_parent(&comment_id), "textOriginal": draft }
        });
        let result = self
            .http
            .post(format!("{API_BASE}/comments"))
            .query(&[("part", "snippet")])
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                let created = response.json::<Created>().await.ok().map(|c| c.id);
                let reply_id = created.filter(|id| is_youtube_id(id));
                sqlx::query(
                    r#"
                    UPDATE community_comments
                    SET status = 'replied', reply_comment_id = $3, replied_at = now(),
                        hold_reason = NULL, updated_at = now()
                    WHERE id = $1 AND workspace_id = $2
                    "#,
                )
                .bind(id)
                .bind(self.workspace_id)
                .bind(reply_id)
                .execute(&self.pool)
                .await?;
                Ok(1)
            }
            Ok(response) if response.status().is_client_error() => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                sqlx::query(
                    "UPDATE community_comments SET status = 'failed', hold_reason = $3, updated_at = now() WHERE id = $1 AND workspace_id = $2",
                )
                .bind(id)
                .bind(self.workspace_id)
                .bind(
                    format!("youtube refused the reply (HTTP {status}): {text}")
                        .chars()
                        .take(500)
                        .collect::<String>(),
                )
                .execute(&self.pool)
                .await?;
                Ok(0)
            }
            other => {
                match other {
                    Ok(response) => {
                        tracing::warn!(status = %response.status(), "youtube reply deferred")
                    }
                    Err(error) => {
                        tracing::warn!(error = %error.without_url(), "youtube reply deferred")
                    }
                }
                sqlx::query(
                    r#"
                    UPDATE community_comments
                    SET status = CASE WHEN $3 >= $4 THEN 'failed' ELSE 'approved' END,
                        hold_reason = CASE WHEN $3 >= $4 THEN 'gave up: youtube kept failing' ELSE hold_reason END,
                        not_before = now() + make_interval(mins => $5),
                        updated_at = now()
                    WHERE id = $1 AND workspace_id = $2
                    "#,
                )
                .bind(id)
                .bind(self.workspace_id)
                .bind(attempts + 1)
                .bind(MAX_ATTEMPTS)
                .bind(RETRY_BACKOFF_MINUTES)
                .execute(&self.pool)
                .await?;
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_flatten_with_the_band_marked_and_replies_under_their_thread() {
        let threads: Vec<Thread> = serde_json::from_value(serde_json::json!([
            {
                "snippet": { "topLevelComment": { "id": "UgzA", "snippet": {
                    "authorDisplayName": "fan1",
                    "authorChannelId": { "value": "UCfan1" },
                    "textOriginal": "what tuning is this?"
                } } },
                "replies": { "comments": [
                    { "id": "UgzA.r1", "snippet": {
                        "authorDisplayName": "Band",
                        "authorChannelId": { "value": "UCband" },
                        "textOriginal": "Drop C!",
                        "parentId": "UgzA"
                    } }
                ] }
            },
            {
                "snippet": { "topLevelComment": { "id": "UgzB", "snippet": {
                    "authorDisplayName": "fan2",
                    "authorChannelId": { "value": "UCfan2" },
                    "textOriginal": "Will you play Kraków?"
                } } }
            }
        ]))
        .expect("threads");
        let flat = flatten("vid123", Some("UCband"), threads);
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[1].parent_id, "UgzA");
        assert!(flat[1].by_band);
        let answer: Vec<&str> = comments_to_answer("vid123", &flat)
            .into_iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(answer, vec!["UgzB"], "the tuning question is answered");
    }

    #[test]
    fn a_reply_to_a_reply_goes_under_the_thread() {
        assert_eq!(thread_parent("UgzA.r1"), "UgzA");
        assert_eq!(thread_parent("UgzB"), "UgzB");
    }

    #[test]
    fn only_youtube_ids_pass() {
        assert!(is_youtube_id("UgzX-y_Z.abc"));
        assert!(!is_youtube_id("../me"));
        assert!(!is_youtube_id("a b"));
        assert!(!is_youtube_id(""));
    }
}
