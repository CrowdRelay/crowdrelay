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
/// Column limit on `content_sources.title` is 240; truncate hard.
const MAX_TITLE_CHARS: usize = 230;
/// How much of a caption becomes the row title — the first line, short.
const TITLE_HEAD_CHARS: usize = 80;
const USER_AGENT: &str = "CrowdRelay/1.0 (social post source sync)";
mod insights;

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
    /// The media the post carries — IG `media_url` (a jpeg for IMAGE, the
    /// mp4 for VIDEO), the first image child's `media_url` for a carousel,
    /// FB `full_picture`. Signed CDN URLs expire; `media_id` is what lets
    /// the executor re-mint a fresh one at post time. Callers that need a
    /// still take `thumbnail_url` first, then this when the type is not
    /// VIDEO.
    pub media_url: Option<String>,
    /// Graph object id `media_url` belongs to — the post's own id for a
    /// single medium, the chosen child's for a carousel. Re-mint path:
    /// `/{media_id}?fields=media_url` (or `thumbnail_url` for a video).
    pub media_id: Option<String>,
    /// IG `media_type` (`IMAGE`/`VIDEO`/`CAROUSEL_ALBUM`); `None` on FB,
    /// where `full_picture` is always a still.
    pub media_type: Option<String>,
    /// The still a VIDEO post shows — kept apart from `media_url` so a video
    /// repost can ship its frame as the photo rather than an unplayable mp4.
    pub thumbnail_url: Option<String>,
    /// Weighted engagement from the same Graph read — likes + comments ×3 +
    /// shares ×5. `None` when the platform reported none of the counts (an
    /// owner can hide likes), which is not zero engagement.
    pub engagement: Option<i64>,
    /// Comments the platform reported, when it did. The reply lane harvests
    /// a post's comments only when this grows past what it last read.
    pub comments_count: Option<i64>,
}

/// Likes + comments ×3 + shares ×5: a comment or share is a person doing
/// something, a like is a thumb. `None` when no count was reported.
pub fn weighted_engagement(
    likes: Option<i64>,
    comments: Option<i64>,
    shares: Option<i64>,
) -> Option<i64> {
    if likes.is_none() && comments.is_none() && shares.is_none() {
        return None;
    }
    Some(
        likes.unwrap_or(0).max(0)
            + 3 * comments.unwrap_or(0).max(0)
            + 5 * shares.unwrap_or(0).max(0),
    )
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
        let configured = self
            .facebook_page_access_token
            .as_ref()
            .ok_or_else(|| "no Facebook Page access token configured".to_owned())?;
        let token = self.page_token(page_id, configured).await;
        let url = format!(
            "{GRAPH_API_BASE}/{page_id}/posts?fields=id,message,created_time,permalink_url,full_picture,reactions.summary(total_count).limit(0),comments.summary(total_count).limit(0),shares&limit={MAX_POSTS_PER_ACCOUNT}&access_token={token}"
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
                // `full_picture` is the post's still — a photo's image, a
                // video's thumbnail. The post id re-mints it via
                // `/{id}?fields=full_picture` when the CDN URL has expired.
                media_url: post.full_picture.clone(),
                media_id: post.full_picture.as_ref().map(|_| post.id.clone()),
                media_type: None,
                thumbnail_url: None,
                engagement: weighted_engagement(
                    post.reactions.as_ref().and_then(GraphCount::total),
                    post.comments.as_ref().and_then(GraphCount::total),
                    post.shares.as_ref().and_then(|shares| shares.count),
                ),
                comments_count: post.comments.as_ref().and_then(GraphCount::total),
            };
            self.upsert_post("facebook", &entry).await?;
        }
        Ok(())
    }

    /// The Page's own access token, read with whatever token is configured.
    ///
    /// `/{page}/posts` answers only to a Page token. Production's configured
    /// credential stopped being one on or after 2026-09-24 and every hourly
    /// sweep failed from then on with `OAuthException` 190/2069032 ("a Page
    /// access token is required") — the band's Facebook posts stopped
    /// arriving, so nothing new from Facebook was relayed. A user token that
    /// manages the Page can read the Page's token from `/{page}?fields=
    /// access_token`; a Page token either answers the same or refuses, and
    /// then the configured token is used as it always was. Read per sweep,
    /// never stored, never logged.
    async fn page_token(&self, page_id: &str, configured: &str) -> String {
        #[derive(Deserialize)]
        struct PageToken {
            access_token: Option<String>,
        }
        let url =
            format!("{GRAPH_API_BASE}/{page_id}?fields=access_token&access_token={configured}");
        let exchanged = match self.http_client.get(&url).send().await {
            Ok(response) if response.status().is_success() => response
                .json::<PageToken>()
                .await
                .ok()
                .and_then(|page| page.access_token)
                .filter(|token| !token.is_empty()),
            _ => None,
        };
        exchanged.unwrap_or_else(|| configured.to_owned())
    }

    /// Instagram Business media — `/{ig_user}/media` under the Page token the
    /// linked account shares.
    async fn sync_instagram(&self, ig_user_id: &str) -> Result<(), String> {
        let token = self
            .facebook_page_access_token
            .as_ref()
            .ok_or_else(|| "no Facebook Page access token configured".to_owned())?;
        let url = format!(
            "{GRAPH_API_BASE}/{ig_user_id}/media?fields=id,caption,timestamp,permalink,media_type,media_url,thumbnail_url,like_count,comments_count,children{{media_url,media_type,thumbnail_url}}&limit={MAX_POSTS_PER_ACCOUNT}&access_token={token}"
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
            // Pick the still a repost can carry. A photo answers its own
            // `media_url`; a video answers `thumbnail_url` (the mp4 cannot be
            // an image post); a carousel answers its first image child's
            // `media_url` — band photo posts are usually carousels, and the
            // parent's media_url is empty. `media_id` names the Graph object
            // the URL came from so the executor can re-mint it at post time.
            let children = media.children.as_ref().map(|c| c.data.as_slice());
            let (media_url, media_id) = match media.media_type.as_deref() {
                // First child with a usable still wins: an IMAGE answers
                // media_url, a VIDEO answers thumbnail_url (the mp4 itself
                // cannot be an image post).
                Some("CAROUSEL_ALBUM") => children
                    .unwrap_or_default()
                    .iter()
                    .find_map(|c| {
                        let url = if c.media_type.as_deref() == Some("VIDEO") {
                            c.thumbnail_url.clone()
                        } else {
                            c.media_url.clone()
                        };
                        url.map(|u| (u, c.id.clone()))
                    })
                    .map(|(u, id)| (Some(u), Some(id)))
                    .unwrap_or((None, None)),
                Some("VIDEO") => (
                    media
                        .thumbnail_url
                        .clone()
                        .or_else(|| media.media_url.clone()),
                    Some(media.id.clone()),
                ),
                _ => (media.media_url.clone(), Some(media.id.clone())),
            };
            let entry = PostEntry {
                external_id: media.id.clone(),
                title,
                url: media.permalink.clone(),
                posted_at: media.timestamp.as_deref().and_then(parse_graph_timestamp),
                caption,
                media_url,
                media_id,
                media_type: media.media_type.clone(),
                thumbnail_url: media.thumbnail_url.clone(),
                engagement: weighted_engagement(media.like_count, media.comments_count, None),
                comments_count: media.comments_count,
            };
            self.upsert_post("instagram", &entry).await?;
            self.refresh_instagram_insights(
                &media.id,
                media.media_type.as_deref(),
                entry.posted_at,
            )
            .await;
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
            // Media carried so the relay can post the original picture, not
            // just the caption. `media_id` is the durable half — the URLs
            // are signed CDN links and expire.
            "media_id": entry.media_id,
            "media_url": entry.media_url,
            "media_type": entry.media_type,
            "thumbnail_url": entry.thumbnail_url,
        });

        let mut tx = self.pool.begin().await.map_err(|e| format!("begin: {e}"))?;
        let upserted: Option<(Uuid, i64)> = sqlx::query_as(
            r#"
            INSERT INTO content_sources (
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
                occurred_at = COALESCE($5, content_sources.occurred_at),
                expires_at = GREATEST(
                    content_sources.expires_at,
                    COALESCE($5, content_sources.occurred_at) + make_interval(days => $7)
                ),
                metadata = content_sources.metadata || EXCLUDED.metadata,
                version = content_sources.version + 1
            WHERE content_sources.title IS DISTINCT FROM EXCLUDED.title
               OR content_sources.occurred_at IS DISTINCT FROM EXCLUDED.occurred_at
               OR content_sources.expires_at IS DISTINCT FROM EXCLUDED.expires_at
               -- Engagement is written separately below and is not an edit;
               -- compared with it, every sync would look like a new version.
               OR (content_sources.metadata - 'engagement' - 'engagement_at'
                   - 'comments_count' - 'comments_harvested'
                   - 'reach' - 'saves' - 'shares' - 'views' - 'avg_watch_ms' - 'insights_at')
                  IS DISTINCT FROM EXCLUDED.metadata
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

        // Engagement moves every hour; it is not an edit of the post. Written
        // on its own so it neither bumps the version nor writes history.
        if let Some(engagement) = entry.engagement {
            sqlx::query(
                r#"
                UPDATE content_sources
                SET metadata = metadata || jsonb_build_object(
                    'engagement', $3::bigint,
                    'engagement_at', to_jsonb(now())
                )
                WHERE workspace_id = $1 AND source_kind = 'social_post' AND source_key = $2
                  AND (metadata->'engagement') IS DISTINCT FROM to_jsonb($3::bigint)
                "#,
            )
            .bind(self.workspace_id)
            .bind(&source_key)
            .bind(engagement)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("engagement: {e}"))?;
        }

        // The comment count, the same way: the reply lane's trigger, not an
        // edit of the post.
        if let Some(comments) = entry.comments_count {
            sqlx::query(
                r#"
                UPDATE content_sources
                SET metadata = metadata || jsonb_build_object('comments_count', $3::bigint)
                WHERE workspace_id = $1 AND source_kind = 'social_post' AND source_key = $2
                  AND (metadata->'comments_count') IS DISTINCT FROM to_jsonb($3::bigint)
                "#,
            )
            .bind(self.workspace_id)
            .bind(&source_key)
            .bind(comments.max(0))
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("comments count: {e}"))?;
        }

        if let Some((source_id, version)) = upserted {
            sqlx::query(
                r#"
                INSERT INTO content_source_history (
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
                FROM content_sources
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
    full_picture: Option<String>,
    reactions: Option<GraphCount>,
    comments: Option<GraphCount>,
    shares: Option<FacebookShares>,
}

/// `{field}.summary(total_count).limit(0)` → `{"summary":{"total_count":N}}`.
#[derive(Debug, Deserialize)]
struct GraphCount {
    summary: Option<GraphSummary>,
}

#[derive(Debug, Deserialize)]
struct GraphSummary {
    total_count: Option<i64>,
}

impl GraphCount {
    fn total(&self) -> Option<i64> {
        self.summary
            .as_ref()
            .and_then(|summary| summary.total_count)
    }
}

#[derive(Debug, Deserialize)]
struct FacebookShares {
    count: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct InstagramMedia {
    id: String,
    caption: Option<String>,
    timestamp: Option<String>,
    permalink: Option<String>,
    media_type: Option<String>,
    media_url: Option<String>,
    thumbnail_url: Option<String>,
    /// Absent when the owner hides like counts.
    like_count: Option<i64>,
    comments_count: Option<i64>,
    children: Option<GraphDataPage<InstagramMediaChild>>,
}

/// A carousel member — same media fields minus the caption/permalink the
/// children endpoint does not repeat.
#[derive(Debug, Deserialize)]
struct InstagramMediaChild {
    id: String,
    media_url: Option<String>,
    media_type: Option<String>,
    thumbnail_url: Option<String>,
}

#[cfg(test)]
mod engagement_tests {
    use super::weighted_engagement;

    #[test]
    fn comments_and_shares_outweigh_likes_and_silence_is_not_zero() {
        assert_eq!(weighted_engagement(Some(10), Some(2), Some(1)), Some(21));
        assert_eq!(weighted_engagement(None, Some(4), None), Some(12));
        assert_eq!(weighted_engagement(None, None, None), None);
    }
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
