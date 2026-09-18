//! Social post source sync: watch each connected owned social account and
//! register the band's own posts as trusted `social_post` content sources.
//!
//! A post the band already made is the cheapest material there is — it
//! happened, it is public, and it is already in the band's voice. Syncing it
//! does two jobs: the watcher sees the band alive (the stop rule counts live
//! material, and a posting band is not a quiet one), and the relay path has
//! the post itself to carry rather than a paraphrase of it.
//!
//! Sources read through the Facebook Page access token — the same credential
//! the growth-metric sync and the post executor already use. Facebook Page
//! posts and the linked Instagram Business account's media both answer to it.
//! An `x` connection with no credential is logged and skipped, never silently
//! dropped: the operator sees the platform listed as not-watched rather than
//! imagining coverage that does not exist.
//!
//! Only owned accounts feed this — the connection itself is the tenant's
//! attestation that the account is theirs, which is exactly the provenance
//! rule the honesty layer requires. No public scraping of other people's
//! posts happens here.
//!
//! Wake paths: a `growth_metric_sync` NOTIFY (the fanbase_connections trigger
//! fires it when a connection appears or changes) and a periodic sweep —
//! Meta does not notify us when the band posts.
//!
//! Crash safety: each upsert is keyed on `(workspace_id, 'social_post',
//! '{platform}:{post_id}')` via ON CONFLICT, so a re-run is a no-op.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, postgres::PgListener};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::watch;
use uuid::Uuid;

/// How often each account is re-read. An hour is fresh enough for "the band
/// posted" without leaning on the Graph API.
const SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Bound on connections read per cycle, per platform.
const MAX_CONNECTIONS_PER_CYCLE: i64 = 10;
/// Bound on posts read per account per cycle.
const MAX_POSTS_PER_ACCOUNT: usize = 25;
/// HTTP timeout for every Graph API call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// A social post stays shareable for 45 days — the same news window the
/// completed-show sources get.
const SOURCE_LIFETIME_DAYS: i64 = 45;
/// The longest caption kept as a voice sample — same bound the video watcher
/// applies to descriptions.
const MAX_CAPTION_CHARS: usize = 1_000;
/// Column limit on `viryaos_content_sources.title` is 240; truncate hard.
const MAX_TITLE_CHARS: usize = 230;
/// How much of a caption becomes the row title — the first line, short.
const TITLE_HEAD_CHARS: usize = 80;
const USER_AGENT: &str = "CrowdRelay/1.0 (social post source sync)";
const GRAPH_API_BASE: &str = "https://graph.facebook.com/v21.0";

#[derive(Debug, Error)]
pub enum SocialPostSourceSyncError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

/// One owned post as the platform reports it — the facts only. Public for
/// the same reason `ReleaseEntry` is — the postgres suite drives the shipped
/// upsert, and placeholder drift fails only against a real schema.
#[derive(Debug)]
pub struct PostEntry {
    /// Stable platform identifier — becomes `{platform}:{id}` source_key.
    pub external_id: String,
    /// First line of the caption, shortened — what the panel reads.
    pub title: String,
    /// Canonical permalink — what the artifact cites.
    pub url: Option<String>,
    pub posted_at: Option<OffsetDateTime>,
    /// The band's own words, full but bounded — voice material.
    pub caption: Option<String>,
}

#[derive(Clone)]
pub struct SocialPostSourceSyncWorker {
    pool: PgPool,
    http_client: reqwest::Client,
    workspace_id: Uuid,
    facebook_page_access_token: Option<String>,
}

impl SocialPostSourceSyncWorker {
    pub fn new(
        pool: PgPool,
        workspace_id: Uuid,
        facebook_page_access_token: Option<String>,
    ) -> Result<Self, SocialPostSourceSyncError> {
        let http_client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT.min(Duration::from_secs(10)))
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(SocialPostSourceSyncError::ClientBuild)?;
        Ok(Self {
            pool,
            http_client,
            workspace_id,
            facebook_page_access_token,
        })
    }

    /// Main loop: initial sweep on startup, then wake on NOTIFY (a connection
    /// appeared or changed) or on the freshness interval.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), SocialPostSourceSyncError> {
        tracing::info!("social post source sync worker started");

        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(SocialPostSourceSyncError::Database)?;
        listener
            .listen("growth_metric_sync")
            .await
            .map_err(SocialPostSourceSyncError::Database)?;

        self.sync_cycle().await;

        let mut tick =
            tokio::time::interval_at(tokio::time::Instant::now() + SYNC_INTERVAL, SYNC_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("social post source sync worker shutting down");
                        return Ok(());
                    }
                }
                _ = listener.recv() => {
                    self.sync_cycle().await;
                }
                _ = tick.tick() => {
                    self.sync_cycle().await;
                }
            }
        }
    }

    /// One cycle: every connected owned account's recent posts become content
    /// sources. Per-connection failures are logged, not propagated.
    async fn sync_cycle(&self) {
        // 'x' is listed on purpose: a connection the operator made and we
        // cannot watch reads as coverage that does not exist. Naming it here
        // is how the honest gap stays visible.
        for platform in ["facebook", "instagram", "x"] {
            let connections: Vec<(Uuid, String)> = match sqlx::query_as(
                r#"
                SELECT id, provider_account_id
                FROM fanbase_connections
                WHERE workspace_id = $1
                  AND platform = $2
                  AND status = 'connected'
                  AND provider_account_id IS NOT NULL
                  AND provider_account_id <> ''
                LIMIT $3
                "#,
            )
            .bind(self.workspace_id)
            .bind(platform)
            .bind(MAX_CONNECTIONS_PER_CYCLE)
            .fetch_all(&self.pool)
            .await
            {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(error = %error, platform, "social post sync: could not list connections");
                    continue;
                }
            };

            if platform == "x" && !connections.is_empty() {
                tracing::info!(
                    count = connections.len(),
                    "social post sync: x connections exist but no X credential is configured — posts are not watched"
                );
                continue;
            }

            for (connection_id, account_id) in connections {
                let result = match platform {
                    "facebook" => self.sync_facebook(&account_id).await,
                    "instagram" => self.sync_instagram(&account_id).await,
                    _ => Ok(()),
                };
                if let Err(error) = result {
                    tracing::warn!(
                        connection_id = %connection_id,
                        platform,
                        account_id = %account_id,
                        error = %error,
                        "social post sync: account sweep failed"
                    );
                }
            }
        }
    }

    /// Facebook Page posts — `/{page}/posts` answers with the band's own
    /// published posts under the Page token.
    async fn sync_facebook(&self, page_id: &str) -> Result<(), String> {
        let token = self
            .facebook_page_access_token
            .as_ref()
            .ok_or_else(|| "no Facebook Page access token configured".to_owned())?;
        let url = format!(
            "{GRAPH_API_BASE}/{page_id}/posts?fields=id,message,created_time,permalink_url&limit={MAX_POSTS_PER_ACCOUNT}&access_token={token}"
        );
        // The token is inside the URL — strip it off transport errors so it
        // cannot reach a log line, the same guard the metric sync applies.
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("posts fetch failed: {}", error.without_url()))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "posts api returned {status}: {}",
                truncate_body(&body)
            ));
        }
        let page: GraphDataPage<FacebookPost> = response
            .json()
            .await
            .map_err(|e| format!("posts body parse failed: {e}"))?;
        for post in &page.data {
            let caption = post
                .message
                .as_deref()
                .map(|c| truncate_chars(c.trim(), MAX_CAPTION_CHARS))
                .filter(|c| !c.is_empty());
            let Some(title) = caption.as_deref().map(post_title).filter(|t| !t.is_empty()) else {
                continue;
            };
            let entry = PostEntry {
                external_id: post.id.clone(),
                title,
                url: post.permalink_url.clone(),
                posted_at: post.created_time.as_deref().and_then(parse_graph_timestamp),
                caption,
            };
            self.upsert_post("facebook", &entry).await?;
        }
        Ok(())
    }

    /// Instagram Business media — `/{ig_user}/media` under the Page token the
    /// linked account shares.
    async fn sync_instagram(&self, ig_user_id: &str) -> Result<(), String> {
        let token = self
            .facebook_page_access_token
            .as_ref()
            .ok_or_else(|| "no Facebook Page access token configured".to_owned())?;
        let url = format!(
            "{GRAPH_API_BASE}/{ig_user_id}/media?fields=id,caption,timestamp,permalink,media_type&limit={MAX_POSTS_PER_ACCOUNT}&access_token={token}"
        );
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("media fetch failed: {}", error.without_url()))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "media api returned {status}: {}",
                truncate_body(&body)
            ));
        }
        let page: GraphDataPage<InstagramMedia> = response
            .json()
            .await
            .map_err(|e| format!("media body parse failed: {e}"))?;
        for media in &page.data {
            let caption = media
                .caption
                .as_deref()
                .map(|c| truncate_chars(c.trim(), MAX_CAPTION_CHARS))
                .filter(|c| !c.is_empty());
            // A photo with no caption is still a post the band made — the
            // title falls back to the media type and the post's date.
            let title = caption
                .as_deref()
                .map(post_title)
                .filter(|t| !t.is_empty())
                .or_else(|| {
                    Some(format!(
                        "Instagram {}",
                        media.media_type.as_deref().unwrap_or("post").to_lowercase()
                    ))
                });
            let Some(title) = title else {
                continue;
            };
            let entry = PostEntry {
                external_id: media.id.clone(),
                title,
                url: media.permalink.clone(),
                posted_at: media.timestamp.as_deref().and_then(parse_graph_timestamp),
                caption,
            };
            self.upsert_post("instagram", &entry).await?;
        }
        Ok(())
    }

    /// Idempotent upsert keyed on `{platform}:{post_id}`. A caption edit
    /// bumps the version and records history; an unchanged row is a no-op
    /// under the IS DISTINCT FROM guard. A post the API never stamped keeps
    /// the anchor it was first filed with instead of restamping "now".
    pub async fn upsert_post(&self, platform: &str, entry: &PostEntry) -> Result<(), String> {
        let source_key = format!("{platform}:{}", entry.external_id);
        let title = truncate_chars(&entry.title, MAX_TITLE_CHARS);
        let metadata = json!({
            "url": entry.url,
            "platform": platform,
            "posted_at": entry.posted_at.map(|t| t.unix_timestamp()),
            "origin": format!("{platform}_graph_api"),
            "body": entry.caption,
        });

        let mut tx = self.pool.begin().await.map_err(|e| format!("begin: {e}"))?;
        let upserted: Option<(Uuid, i64)> = sqlx::query_as(
            r#"
            INSERT INTO viryaos_content_sources (
                id, workspace_id, source_kind, source_key, title,
                occurred_at, expires_at, metadata
            ) VALUES (
                $3, $1, 'social_post', $2, $4,
                COALESCE($5, now()),
                COALESCE($5, now()) + make_interval(days => $7),
                $6
            )
            ON CONFLICT (workspace_id, source_kind, source_key) DO UPDATE SET
                title = EXCLUDED.title,
                occurred_at = COALESCE($5, viryaos_content_sources.occurred_at),
                expires_at = GREATEST(
                    viryaos_content_sources.expires_at,
                    COALESCE($5, viryaos_content_sources.occurred_at) + make_interval(days => $7)
                ),
                metadata = viryaos_content_sources.metadata || EXCLUDED.metadata,
                version = viryaos_content_sources.version + 1
            WHERE viryaos_content_sources.title IS DISTINCT FROM EXCLUDED.title
               OR viryaos_content_sources.occurred_at IS DISTINCT FROM EXCLUDED.occurred_at
               OR viryaos_content_sources.expires_at IS DISTINCT FROM EXCLUDED.expires_at
               OR viryaos_content_sources.metadata IS DISTINCT FROM EXCLUDED.metadata
            RETURNING id, version
            "#,
        )
        .bind(self.workspace_id)
        .bind(&source_key)
        .bind(Uuid::now_v7())
        .bind(&title)
        .bind(entry.posted_at)
        .bind(&metadata)
        .bind(SOURCE_LIFETIME_DAYS as i32)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("upsert: {e}"))?;

        if let Some((source_id, version)) = upserted {
            sqlx::query(
                r#"
                INSERT INTO viryaos_content_source_history (
                    workspace_id, source_id, version, snapshot
                )
                SELECT workspace_id, id, version, jsonb_build_object(
                    'source_kind', source_kind,
                    'source_key', source_key,
                    'title', title,
                    'occurred_at', occurred_at,
                    'expires_at', expires_at,
                    'metadata', metadata,
                    'active', active,
                    'format_key', format_key
                )
                FROM viryaos_content_sources
                WHERE workspace_id = $1 AND id = $2 AND version = $3
                "#,
            )
            .bind(self.workspace_id)
            .bind(source_id)
            .bind(version)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("history: {e}"))?;
        }

        tx.commit().await.map_err(|e| format!("commit: {e}"))?;
        Ok(())
    }
}

// ----------------------------------------------------------------------
// Graph API shapes — the fields this worker reads, nothing more
// ----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GraphDataPage<T> {
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct FacebookPost {
    id: String,
    message: Option<String>,
    created_time: Option<String>,
    permalink_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InstagramMedia {
    id: String,
    caption: Option<String>,
    timestamp: Option<String>,
    permalink: Option<String>,
    media_type: Option<String>,
}

/// First line of a caption, shortened — the panel's readable name for a post.
fn post_title(caption: &str) -> String {
    let first_line = caption.lines().next().unwrap_or_default().trim();
    truncate_chars(first_line, TITLE_HEAD_CHARS)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        text.chars().take(max).collect()
    } else {
        text.to_owned()
    }
}

/// Truncate an error body so a provider rant cannot flood the log — same
/// bound the metric sync applies to Graph API errors.
fn truncate_body(body: &str) -> String {
    truncate_chars(body, 300)
}

/// The Graph API stamps `2026-09-15T18:00:00+0000` — RFC3339 with a bare
/// offset, no colon. Normalize the offset before parsing so a real timestamp
/// is never dropped for formatting alone.
fn parse_graph_timestamp(raw: &str) -> Option<OffsetDateTime> {
    let mut value = raw.to_owned();
    // A bare offset ends "+0000" — sign at len-5, then four digits.
    let tail = value.len().saturating_sub(5);
    if value.len() >= 5
        && matches!(value.as_bytes().get(tail).copied(), Some(b'+' | b'-'))
        && value
            .get(tail + 1..)
            .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
    {
        value.insert(value.len() - 2, ':');
    }
    OffsetDateTime::parse(&value, &Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_title_takes_the_first_line() {
        assert_eq!(
            post_title("New single Friday\nLink in bio"),
            "New single Friday"
        );
        assert_eq!(post_title(""), "");
        let long = "a".repeat(200);
        assert_eq!(post_title(&long).chars().count(), TITLE_HEAD_CHARS);
    }

    #[test]
    fn facebook_post_parses() {
        let body = r#"{"data": [{"id": "123_456", "message": "New single out now", "created_time": "2026-09-15T18:00:00+0000", "permalink_url": "https://facebook.com/123/posts/456"}]}"#;
        let page: GraphDataPage<FacebookPost> = serde_json::from_str(body).unwrap();
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].id, "123_456");
        assert!(page.data[0].permalink_url.is_some());
    }

    #[test]
    fn instagram_media_parses() {
        let body = r#"{"data": [{"id": "1790", "caption": "studio day", "timestamp": "2026-09-14T10:00:00+0000", "permalink": "https://instagram.com/p/xyz", "media_type": "IMAGE"}]}"#;
        let page: GraphDataPage<InstagramMedia> = serde_json::from_str(body).unwrap();
        assert_eq!(page.data[0].caption.as_deref(), Some("studio day"));
    }

    #[test]
    fn graph_timestamp_normalizes_bare_offset() {
        assert!(parse_graph_timestamp("2026-09-15T18:00:00+0000").is_some());
        assert!(parse_graph_timestamp("2026-09-15T18:00:00+00:00").is_some());
        assert!(parse_graph_timestamp("not a date").is_none());
    }
}
