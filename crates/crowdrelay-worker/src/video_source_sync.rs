//! Video source sync: watch each connected YouTube channel's public Atom feed
//! and register new uploads as trusted `video` content sources.
//!
//! This is how a new music video reaches the community loop without a human
//! pasting a link: the feed entry becomes a `viryaos_content_sources` row the
//! engager may draft from, and the content-supply evaluator schedules its
//! artifacts. The watcher only records facts the feed carries — video id,
//! title, publish time, link — never a story around them.
//!
//! Wake paths: a `growth_metric_sync` NOTIFY (the fanbase_connections trigger
//! fires it when a YouTube connection appears or changes) and a periodic
//! sweep for feed freshness — YouTube does not notify us when a video lands.
//!
//! Crash safety: each upsert is keyed on `(workspace_id, 'video',
//! 'youtube:{video_id}')` via ON CONFLICT, so a re-run of the same feed is a
//! no-op and a crash mid-cycle is reclaimed by the next one.

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, postgres::PgListener};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{sync::watch, time::interval};
use uuid::Uuid;

/// How often each channel's feed is re-read. Thirty minutes is fresh enough
/// for "new video" without leaning on a public endpoint.
const SYNC_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Bound on channels read per cycle — one workspace should never have many.
const MAX_CHANNELS_PER_CYCLE: i64 = 10;
/// Bound on entries read per feed — a channel's Atom feed lists its latest
/// ~15 uploads; anything older was already captured or is not new.
const MAX_ENTRIES_PER_FEED: usize = 25;
/// HTTP timeout for the feed fetch.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// A video stays shareable for a year after upload. Videos are evergreen
/// sources — the content-supply evaluator does not age them out by
/// `occurred_at`, so this expiry is the whole freshness bound.
const SOURCE_LIFETIME_DAYS: i64 = 365;
const USER_AGENT: &str = "CrowdRelay/1.0 (video source sync)";

#[derive(Debug, Error)]
pub enum VideoSourceSyncError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct VideoSourceSyncWorker {
    pool: PgPool,
    http_client: reqwest::Client,
    workspace_id: Uuid,
}

/// One `<entry>` from a channel's Atom feed.
#[derive(Debug)]
struct FeedEntry {
    video_id: String,
    title: String,
    published: Option<OffsetDateTime>,
}

impl VideoSourceSyncWorker {
    pub fn new(pool: PgPool, workspace_id: Uuid) -> Result<Self, VideoSourceSyncError> {
        let http_client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT.min(Duration::from_secs(10)))
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            // The shorts probe needs the raw status: a full video redirects
            // /shorts/{id} to /watch, and a followed redirect would hide that.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(VideoSourceSyncError::ClientBuild)?;
        Ok(Self {
            pool,
            http_client,
            workspace_id,
        })
    }

    /// Main loop: initial sweep on startup, then wake on NOTIFY (a YouTube
    /// connection appeared or changed) or on the freshness interval.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), VideoSourceSyncError> {
        tracing::info!("video source sync worker started");

        // Same channel the fanbase_connections trigger notifies — a new
        // YouTube connection should be read immediately, not in 30 minutes.
        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(VideoSourceSyncError::Database)?;
        listener
            .listen("growth_metric_sync")
            .await
            .map_err(VideoSourceSyncError::Database)?;

        self.sync_cycle().await;

        let mut tick = interval(SYNC_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("video source sync worker shutting down");
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

    /// One cycle: every connected YouTube channel's latest uploads become
    /// content sources. Per-channel failures are logged, not propagated —
    /// one unreachable feed must not starve the others.
    async fn sync_cycle(&self) {
        let channels: Vec<(Uuid, String)> = match sqlx::query_as(
            r#"
            SELECT id, provider_account_id
            FROM fanbase_connections
            WHERE workspace_id = $1
              AND platform = 'youtube'
              AND status = 'connected'
              AND provider_account_id IS NOT NULL
              AND provider_account_id <> ''
            LIMIT $2
            "#,
        )
        .bind(self.workspace_id)
        .bind(MAX_CHANNELS_PER_CYCLE)
        .fetch_all(&self.pool)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!(error = %error, "video source sync: could not list channels");
                return;
            }
        };

        for (connection_id, channel_id) in channels {
            if let Err(error) = self.sync_channel(&channel_id).await {
                tracing::warn!(
                    connection_id = %connection_id,
                    channel_id = %channel_id,
                    error = %error,
                    "video source sync: channel sweep failed"
                );
            }
        }
    }

    async fn sync_channel(&self, channel_id: &str) -> Result<(), String> {
        let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");
        let body = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("feed fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("feed returned {e}"))?
            .text()
            .await
            .map_err(|e| format!("feed body read failed: {e}"))?;

        let entries = parse_feed(&body);
        for entry in entries.into_iter().take(MAX_ENTRIES_PER_FEED) {
            // Only full videos become share sources — a Short is a format the
            // community strategy never turns into a thread post. The probe
            // runs once per unseen id; a known row is never re-checked.
            let source_key = format!("youtube:{}", entry.video_id);
            if !self.source_exists(&source_key).await? && self.is_short(&entry.video_id).await {
                tracing::info!(
                    video_id = %entry.video_id,
                    "video source sync: skipped a short"
                );
                continue;
            }
            self.upsert_video(channel_id, &entry).await?;
        }
        Ok(())
    }

    /// Whether a `youtube:{id}` source row already exists for this workspace.
    async fn source_exists(&self, source_key: &str) -> Result<bool, String> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM viryaos_content_sources
                WHERE workspace_id = $1
                  AND source_kind = 'video'
                  AND source_key = $2
            )
            "#,
        )
        .bind(self.workspace_id)
        .bind(source_key)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("source exists check: {e}"))
    }

    /// YouTube answers `/shorts/{id}` with 200 for a Short and redirects a
    /// full video to `/watch`. A probe failure must not drop a real upload
    /// forever — treat it as a full video and let the row live or die on
    /// the content rules, not on one transient HTTP error.
    async fn is_short(&self, video_id: &str) -> bool {
        let url = format!("https://www.youtube.com/shorts/{video_id}");
        match self.http_client.get(&url).send().await {
            Ok(response) => is_short_status(response.status()),
            Err(_) => false,
        }
    }

    /// Idempotent upsert keyed on the video id. A title change bumps the
    /// version and records history; an unchanged row is a no-op so a re-read
    /// of the same feed writes nothing.
    async fn upsert_video(&self, channel_id: &str, entry: &FeedEntry) -> Result<(), String> {
        let source_key = format!("youtube:{}", entry.video_id);
        let occurred_at = entry.published.unwrap_or_else(OffsetDateTime::now_utc);
        let expires_at = occurred_at + time::Duration::days(SOURCE_LIFETIME_DAYS);
        // Column limit is 240; leave room for nothing — truncate hard.
        let title: String = entry.title.chars().take(230).collect();
        if title.trim().is_empty() {
            return Ok(());
        }
        let metadata = json!({
            "url": format!("https://youtu.be/{}", entry.video_id),
            "video_id": entry.video_id,
            "channel_id": channel_id,
            "published_at": entry.published.map(|t| t.unix_timestamp()),
            "origin": "youtube_feed",
        });

        let mut tx = self.pool.begin().await.map_err(|e| format!("begin: {e}"))?;

        // An existing row only bumps version when the facts changed (the
        // WHERE on DO UPDATE), so re-reading the same feed writes nothing and
        // produces no history spam.
        let upserted: Option<(Uuid, i64)> = sqlx::query_as(
            r#"
            INSERT INTO viryaos_content_sources (
                id, workspace_id, source_kind, source_key, title,
                occurred_at, expires_at, metadata
            ) VALUES ($3, $1, 'video', $2, $4, $5, $6, $7)
            ON CONFLICT (workspace_id, source_kind, source_key) DO UPDATE SET
                title = EXCLUDED.title,
                occurred_at = EXCLUDED.occurred_at,
                expires_at = EXCLUDED.expires_at,
                metadata = EXCLUDED.metadata,
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
        .bind(occurred_at)
        .bind(expires_at)
        .bind(&metadata)
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
                    'active', active
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

/// Extracts `<entry>` items from a YouTube channel Atom feed.
///
/// The feed is regular: every entry carries `<yt:videoId>`, `<title>` and
/// `<published>` in a fixed namespace layout. A bounded tag scan is enough —
/// introducing an XML parser for three fields of a 15-entry document is not.
fn parse_feed(body: &str) -> Vec<FeedEntry> {
    let mut entries = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<entry>") {
        let Some(after_open) = rest.get(start + "<entry>".len()..) else {
            break;
        };
        let Some(end) = after_open.find("</entry>") else {
            break;
        };
        let Some(block) = after_open.get(..end) else {
            break;
        };
        let video_id = extract_tag(block, "yt:videoId");
        let title = extract_tag(block, "title");
        let published = extract_tag(block, "published").and_then(|s| {
            OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok()
        });
        if let (Some(video_id), Some(title)) = (video_id, title) {
            entries.push(FeedEntry {
                video_id,
                title,
                published,
            });
        }
        let Some(next) = after_open.get(end + "</entry>".len()..) else {
            break;
        };
        rest = next;
    }
    entries
}

/// The shorts probe's answer, decided on status alone: YouTube serves the
/// /shorts/{id} URL for a Short and redirects a full video to /watch.
fn is_short_status(status: reqwest::StatusCode) -> bool {
    status.is_success()
}

/// Reads the text of the first `<tag>…</tag>` in a block. Handles the
/// namespaced forms the feed uses (`<yt:videoId>`) and CDATA/plain bodies.
fn extract_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let rest = block.get(start..)?;
    let end = rest.find(&close)?;
    let inner = rest.get(..end)?.trim();
    let inner = inner
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(inner);
    let inner = inner.trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns="http://www.w3.org/2005/Atom">
  <title>Virya</title>
  <entry>
    <yt:videoId>abc123XYZ_-</yt:videoId>
    <title>Virya — Ashes (Official Video)</title>
    <published>2026-09-10T17:00:00+00:00</published>
  </entry>
  <entry>
    <yt:videoId>def456</yt:videoId>
    <title><![CDATA[Live at Mystic Festival]]></title>
    <published>2026-08-02T12:00:00+00:00</published>
  </entry>
  <entry>
    <title>no video id — skipped</title>
  </entry>
</feed>"#;

    #[test]
    fn parses_entries_and_skips_incomplete() {
        let entries = parse_feed(SAMPLE_FEED);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].video_id, "abc123XYZ_-");
        assert_eq!(entries[0].title, "Virya — Ashes (Official Video)");
        assert_eq!(entries[1].title, "Live at Mystic Festival");
        assert!(entries[0].published.is_some());
    }

    #[test]
    fn empty_feed_yields_no_entries() {
        assert!(parse_feed("<feed><title>x</title></feed>").is_empty());
    }

    #[test]
    fn shorts_answer_200_full_videos_redirect() {
        use reqwest::StatusCode;
        assert!(is_short_status(StatusCode::OK));
        assert!(!is_short_status(StatusCode::SEE_OTHER));
        assert!(!is_short_status(StatusCode::FOUND));
        assert!(!is_short_status(StatusCode::NOT_FOUND));
    }
}
