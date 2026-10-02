//! Video source sync: watch each connected YouTube channel's uploads and
//! register new ones as trusted `video` content sources.
//!
//! Uploads are read from the Data API's uploads playlist when
//! `CROWDRELAY_YOUTUBE_API_KEY` is set, and from the public Atom feed
//! otherwise. The feed was the only path until 2026-09-25, when
//! `youtube.com/feeds/videos.xml` answered 404 for every channel, Google's
//! own included, from production and from a residential line alike. Every
//! sweep failed, so no new video reached the community loop. The API costs
//! one quota unit per channel per sweep and carries the same fields.
//!
//! This is how a new music video reaches the community loop without a human
//! pasting a link: the feed entry becomes a `content_sources` row the
//! engager may draft from, and the content-supply evaluator schedules its
//! artifacts. The watcher only records facts the feed carries — video id,
//! title, publish time, link, and the description the band typed under the
//! video — never a story around them.
//!
//! The description is the one piece of real prose this feed carries, and it
//! was dropped until now. That mattered more than it looks. The agent
//! templates' VOICE rule tells the model to match the band's own writing, and
//! what a `video` source actually held was a title, a URL and a channel id —
//! not a word anybody wrote. A model asked to match a voice with no sample of
//! it does not write plainly; it invents a personality. The description lands
//! in `metadata.body`, the same key the console's story panel writes, so
//! `list_voice_samples` picks it up without a second code path.
//!
//! Wake paths: a `growth_metric_sync` NOTIFY (the fanbase_connections trigger
//! fires it when a YouTube connection appears or changes) and a periodic
//! sweep for feed freshness — YouTube does not notify us when a video lands.
//!
//! Crash safety: each upsert is keyed on `(workspace_id, 'video',
//! 'youtube:{video_id}')` via ON CONFLICT, so a re-run of the same feed is a
//! no-op and a crash mid-cycle is reclaimed by the next one.

use std::time::Duration;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{AutopilotTeamStateRepository, UpsertReleasePlan};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
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
    /// The Data API key the metric sync already reads with. Present, uploads
    /// come from the API; absent, from the Atom feed.
    youtube_api_key: Option<String>,
    /// The autopilot repository a first-seen upload asks for a release plan.
    autopilot: PostgresAutopilotRepository,
}

/// The longest description kept as a voice sample.
///
/// A YouTube description runs to 5,000 characters and the tail is usually
/// credits, links, label boilerplate and a hashtag block — none of which is
/// how the band writes a sentence. The opening paragraphs are, and they are
/// what a reader sees before "show more". Keeping the head bounds the prompt
/// cost and drops the part that would teach the model to write in hashtags.
const MAX_DESCRIPTION_CHARS: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum YoutubeVideoFormat {
    LongForm,
    Short,
    Unknown,
}

/// One `<entry>` from a channel's Atom feed.
#[derive(Debug)]
pub struct FeedEntry {
    pub video_id: String,
    pub title: String,
    pub published: Option<OffsetDateTime>,
    /// What the band typed under the video. `None` when the feed carries no
    /// `<media:description>` or it is blank — an absent description is absent,
    /// never an empty string, so `list_voice_samples` can exclude it on the
    /// same `btrim(...) <> ''` test it applies to every other source.
    pub description: Option<String>,
}

impl VideoSourceSyncWorker {
    pub fn new(
        pool: PgPool,
        workspace_id: Uuid,
        youtube_api_key: Option<String>,
        autopilot: PostgresAutopilotRepository,
    ) -> Result<Self, VideoSourceSyncError> {
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
            youtube_api_key,
            autopilot,
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
        let entries = match self.youtube_api_key.as_deref() {
            Some(key) => self.uploads_via_api(channel_id, key).await?,
            None => self.uploads_via_feed(channel_id).await?,
        };
        for entry in entries.into_iter().take(MAX_ENTRIES_PER_FEED) {
            // Every upload is re-classified, including rows captured before
            // the Shorts filter existed. That is what lets a later sweep
            // retire historical Shorts instead of trusting "already exists".
            let source_key = format!("youtube:{}", entry.video_id);
            match self.video_format(&entry.video_id).await {
                YoutubeVideoFormat::Short => {
                    self.retire_youtube_short(&entry.video_id).await?;
                    tracing::info!(
                        video_id = %entry.video_id,
                        "video source sync: retired a YouTube Short from promotion"
                    );
                }
                YoutubeVideoFormat::LongForm => {
                    self.upsert_video(channel_id, &entry).await?;
                }
                YoutubeVideoFormat::Unknown => {
                    let known = self.source_exists(&source_key).await?;
                    tracing::warn!(
                        video_id = %entry.video_id,
                        known_source = known,
                        "video source sync: YouTube format unresolved; refusing to open or refresh promotion"
                    );
                    // New unknown uploads are fail-closed. Existing rows are
                    // left untouched until a later sweep can classify them;
                    // crucially, no fresh release plan is opened here.
                }
            }
        }
        Ok(())
    }

    async fn uploads_via_feed(&self, channel_id: &str) -> Result<Vec<FeedEntry>, String> {
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
        Ok(parse_feed(&body))
    }

    /// The channel's uploads playlist through the Data API. A channel id is
    /// `UC…` and its uploads playlist is the same id with `UU`; anything else
    /// is not a channel id this can read. The key rides in the query string,
    /// so every error is stripped of its URL before it can reach a log.
    async fn uploads_via_api(&self, channel_id: &str, key: &str) -> Result<Vec<FeedEntry>, String> {
        let Some(suffix) = channel_id.strip_prefix("UC") else {
            return Err(format!("not a YouTube channel id: {channel_id}"));
        };
        let response = self
            .http_client
            .get("https://www.googleapis.com/youtube/v3/playlistItems")
            .query(&[
                ("part", "snippet,contentDetails"),
                ("maxResults", "25"),
                ("playlistId", &format!("UU{suffix}")),
                ("key", key),
            ])
            .send()
            .await
            .map_err(|e| format!("uploads fetch failed: {}", e.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("YouTube playlistItems API returned HTTP {status}"));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("uploads body read failed: {}", e.without_url()))?;
        Ok(parse_playlist_items(&body))
    }

    /// Whether a `youtube:{id}` source row already exists for this workspace.
    async fn source_exists(&self, source_key: &str) -> Result<bool, String> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM content_sources
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

    /// YouTube answers `/shorts/{id}` with 2xx for a Short and redirects a
    /// full video to `/watch`. Anything else is unknown and must not open a
    /// new promotion/release plan: a transient probe problem is safer than
    /// emailing another band a Short.
    async fn video_format(&self, video_id: &str) -> YoutubeVideoFormat {
        let url = format!("https://www.youtube.com/shorts/{video_id}");
        match self.http_client.get(&url).send().await {
            Ok(response) => youtube_format_from_status(response.status()),
            Err(_) => YoutubeVideoFormat::Unknown,
        }
    }

    /// Remove one confirmed Short from every automatic promotion root.
    ///
    /// Old versions of the watcher could persist a Short and even open a
    /// release plan before format classification existed. A source-level
    /// `active=false` is not enough because outreach/email supply also reads
    /// release plans and campaigns. Retire all three in one transaction.
    ///
    /// # Errors
    /// Returns a stringified database error.
    pub async fn retire_youtube_short(&self, video_id: &str) -> Result<(), String> {
        let source_key = format!("youtube:{video_id}");
        let short_marker = json!("short");
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("begin short retire: {e}"))?;

        let changed_source: Option<(Uuid, i64)> = sqlx::query_as(
            r#"
            UPDATE content_sources
            SET active = false,
                expires_at = LEAST(expires_at, now()),
                metadata = jsonb_set(
                    COALESCE(metadata, '{}'::jsonb),
                    '{youtube_format}',
                    $3::jsonb,
                    true
                ),
                version = version + 1
            WHERE workspace_id = $1
              AND source_kind = 'video'
              AND source_key = $2
              AND (
                  active
                  OR metadata->>'youtube_format' IS DISTINCT FROM 'short'
              )
            RETURNING id, version
            "#,
        )
        .bind(self.workspace_id)
        .bind(&source_key)
        .bind(&short_marker)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("retire short source: {e}"))?;

        if let Some((source_id, version)) = changed_source {
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
            .map_err(|e| format!("retire short history: {e}"))?;
        }

        // Disable campaigns first while their release-plan relation is still
        // visible. This prevents an already-created release campaign from
        // surviving after the source itself is retired.
        sqlx::query(
            r#"
            UPDATE campaigns AS campaign
            SET active = false,
                updated_at = now()
            WHERE campaign.workspace_id = $1
              AND campaign.active
              AND EXISTS (
                  SELECT 1
                  FROM release_plans AS plan
                  WHERE plan.workspace_id = campaign.workspace_id
                    AND plan.id = campaign.release_plan_id
                    AND (
                        plan.source_key = $2
                        OR plan.listen_url IN (
                            $3,
                            $4,
                            $5
                        )
                    )
              )
            "#,
        )
        .bind(self.workspace_id)
        .bind(&source_key)
        .bind(format!("https://youtu.be/{video_id}"))
        .bind(format!("https://www.youtube.com/watch?v={video_id}"))
        .bind(format!("https://www.youtube.com/shorts/{video_id}"))
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("retire short campaigns: {e}"))?;

        sqlx::query(
            r#"
            UPDATE release_plans
            SET active = false,
                communication_enabled = false,
                press_enabled = false,
                version = version + 1
            WHERE workspace_id = $1
              AND (
                  source_key = $2
                  OR listen_url IN ($3, $4, $5)
              )
              AND (
                  active
                  OR communication_enabled
                  OR press_enabled
              )
            "#,
        )
        .bind(self.workspace_id)
        .bind(&source_key)
        .bind(format!("https://youtu.be/{video_id}"))
        .bind(format!("https://www.youtube.com/watch?v={video_id}"))
        .bind(format!("https://www.youtube.com/shorts/{video_id}"))
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("retire short release plans: {e}"))?;

        tx.commit()
            .await
            .map_err(|e| format!("commit short retire: {e}"))?;
        Ok(())
    }

    /// Idempotent upsert keyed on the video id. A title change bumps the
    /// version and records history; an unchanged row is a no-op so a re-read
    /// of the same feed writes nothing.
    pub async fn upsert_video(&self, channel_id: &str, entry: &FeedEntry) -> Result<(), String> {
        let source_key = format!("youtube:{}", entry.video_id);
        let occurred_at = entry.published.unwrap_or_else(OffsetDateTime::now_utc);
        let expires_at = occurred_at + time::Duration::days(SOURCE_LIFETIME_DAYS);
        // Column limit is 240; leave room for nothing — truncate hard.
        let title: String = entry.title.chars().take(230).collect();
        if title.trim().is_empty() {
            return Ok(());
        }
        // `body` carries the band's own description. The key is shared with
        // the console's story panel on purpose: `list_voice_samples` selects
        // `metadata->>'body'` and does not care which writer put it there, so
        // one key means one voice path rather than two that can diverge.
        let metadata = json!({
            "url": format!("https://youtu.be/{}", entry.video_id),
            "video_id": entry.video_id,
            "channel_id": channel_id,
            "published_at": entry.published.map(|t| t.unix_timestamp()),
            "origin": "youtube_feed",
            "youtube_format": "long_form",
            "body": entry.description,
        });

        let mut tx = self.pool.begin().await.map_err(|e| format!("begin: {e}"))?;

        // An existing row only bumps version when the facts changed (the
        // WHERE on DO UPDATE), so re-reading the same feed writes nothing and
        // produces no history spam. `xmax = 0` marks the first insert — the
        // one moment a video is new enough to deserve a release plan.
        let upserted: Option<(Uuid, i64, bool)> = sqlx::query_as(
            r#"
            INSERT INTO content_sources (
                id, workspace_id, source_kind, source_key, title,
                occurred_at, expires_at, metadata
            ) VALUES ($3, $1, 'video', $2, $4, $5, $6, $7)
            ON CONFLICT (workspace_id, source_kind, source_key) DO UPDATE SET
                title = EXCLUDED.title,
                occurred_at = EXCLUDED.occurred_at,
                expires_at = EXCLUDED.expires_at,
                metadata = content_sources.metadata || EXCLUDED.metadata,
                version = content_sources.version + 1
            WHERE content_sources.title IS DISTINCT FROM EXCLUDED.title
               OR content_sources.occurred_at IS DISTINCT FROM EXCLUDED.occurred_at
               OR content_sources.expires_at IS DISTINCT FROM EXCLUDED.expires_at
               OR content_sources.metadata IS DISTINCT FROM (content_sources.metadata || EXCLUDED.metadata)
            RETURNING id, version, (xmax = 0)
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

        let first_insert = upserted.as_ref().is_some_and(|(_, _, inserted)| *inserted);
        if let Some((source_id, version, _)) = upserted {
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
        if first_insert {
            self.maybe_open_release_plan(&title, occurred_at, &entry.video_id)
                .await?;
        }
        Ok(())
    }

    /// A just-uploaded video opens its own release plan — once, and only
    /// while the upload is still fresh. Three days is the window in which a
    /// release-day push is still worth anything; an older video surfaced by
    /// a first feed read is catalogue, not a release.
    ///
    /// A plan already carrying this video's listen url was entered by an
    /// operator (or an earlier watcher run) — the watcher defers to it
    /// rather than inserting a second row under the same key, which the
    /// upsert would merge into anyway.
    async fn maybe_open_release_plan(
        &self,
        title: &str,
        occurred_at: OffsetDateTime,
        video_id: &str,
    ) -> Result<(), String> {
        if occurred_at < OffsetDateTime::now_utc() - time::Duration::days(3) {
            return Ok(());
        }
        let listen_url = format!("https://youtu.be/{video_id}");
        let plan_exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM release_plans
                WHERE workspace_id = $1
                  AND active
                  AND listen_url IN ($2, $3)
            )
            "#,
        )
        .bind(self.workspace_id)
        .bind(&listen_url)
        .bind(format!("https://www.youtube.com/watch?v={video_id}"))
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("release plan check: {e}"))?;
        if plan_exists {
            return Ok(());
        }
        let idempotency_key = IdempotencyKey::parse(format!("video-watcher:youtube:{video_id}"))
            .map_err(|e| format!("video-watcher key: {e}"))?;
        self.autopilot
            .upsert_release_plan(
                WorkspaceId::from_uuid(self.workspace_id),
                UpsertReleasePlan {
                    release_id: None,
                    source_key: format!("youtube:{video_id}"),
                    title: title.to_owned(),
                    release_at: occurred_at,
                    listen_url: Some(listen_url),
                    tier: None,
                    active: true,
                    assets_ready: true,
                    communication_enabled: true,
                    press_enabled: true,
                    expected_version: 0,
                },
                "video-watcher",
                &idempotency_key,
                None,
            )
            .await
            .map_err(|e| format!("release plan upsert: {e}"))?;
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
        // The description sits inside `<media:group>`, and `extract_tag` finds
        // the first match in the whole entry block, which is the right one —
        // an entry carries exactly one. Truncation counts characters rather
        // than bytes so a Polish or emoji-carrying description is never cut
        // mid-codepoint.
        let description = extract_tag(block, "media:description").map(|text| {
            if text.chars().count() > MAX_DESCRIPTION_CHARS {
                text.chars().take(MAX_DESCRIPTION_CHARS).collect()
            } else {
                text
            }
        });
        if let (Some(video_id), Some(title)) = (video_id, title) {
            entries.push(FeedEntry {
                video_id,
                title,
                published,
                description,
            });
        }
        let Some(next) = after_open.get(end + "</entry>".len()..) else {
            break;
        };
        rest = next;
    }
    entries
}

/// The same entries from a Data API `playlistItems` response.
///
/// `contentDetails.videoPublishedAt` is when the video went public;
/// `snippet.publishedAt` is when it joined the playlist, which for the
/// uploads playlist is the upload, not the release. A private or deleted
/// item carries no `videoPublishedAt` and is skipped, the same as a feed
/// entry missing its id: it is not a video anyone can be sent to.
fn parse_playlist_items(body: &serde_json::Value) -> Vec<FeedEntry> {
    let Some(items) = body.get("items").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let snippet = item.get("snippet")?;
            let video_id = snippet
                .get("resourceId")?
                .get("videoId")?
                .as_str()?
                .trim()
                .to_owned();
            let title = snippet.get("title")?.as_str()?.trim().to_owned();
            let published = item
                .get("contentDetails")?
                .get("videoPublishedAt")?
                .as_str()
                .and_then(|value| {
                    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                        .ok()
                })?;
            let description = snippet
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(MAX_DESCRIPTION_CHARS).collect());
            (!video_id.is_empty() && !title.is_empty()).then_some(FeedEntry {
                video_id,
                title,
                published: Some(published),
                description,
            })
        })
        .collect()
}

/// The Shorts probe is deliberately tri-state. A probe problem must never be
/// interpreted as permission to promote a fresh upload.
fn youtube_format_from_status(status: reqwest::StatusCode) -> YoutubeVideoFormat {
    if status.is_success() {
        YoutubeVideoFormat::Short
    } else if status.is_redirection() {
        YoutubeVideoFormat::LongForm
    } else {
        YoutubeVideoFormat::Unknown
    }
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
    <media:group>
      <media:description>Wrote this one in a week. Recorded it in two.</media:description>
    </media:group>
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

    /// The Data API path yields the same entries the feed did: the band's
    /// description, the release time rather than the playlist time, and no
    /// private or deleted uploads.
    #[test]
    fn playlist_items_become_the_same_entries() {
        let long = "ą".repeat(MAX_DESCRIPTION_CHARS + 10);
        let body = serde_json::json!({
            "items": [
                {
                    "snippet": {
                        "title": " Virya — Ashes (Official Video) ",
                        "description": "Wrote this one in a week.",
                        "publishedAt": "2026-09-01T00:00:00Z",
                        "resourceId": { "kind": "youtube#video", "videoId": "abc123XYZ_-" }
                    },
                    "contentDetails": { "videoId": "abc123XYZ_-", "videoPublishedAt": "2026-08-30T18:00:00Z" }
                },
                {
                    "snippet": {
                        "title": "Private video",
                        "description": "This video is private.",
                        "resourceId": { "kind": "youtube#video", "videoId": "hidden00001" }
                    },
                    "contentDetails": { "videoId": "hidden00001" }
                },
                {
                    "snippet": {
                        "title": "Live at Mystic Festival",
                        "description": "   ",
                        "resourceId": { "kind": "youtube#video", "videoId": "live0000001" }
                    },
                    "contentDetails": { "videoId": "live0000001", "videoPublishedAt": "2026-08-02T12:00:00Z" }
                },
                {
                    "snippet": {
                        "title": "Long notes",
                        "description": long,
                        "resourceId": { "kind": "youtube#video", "videoId": "long0000001" }
                    },
                    "contentDetails": { "videoId": "long0000001", "videoPublishedAt": "2026-07-02T12:00:00Z" }
                }
            ]
        });
        let entries = parse_playlist_items(&body);
        assert_eq!(entries.len(), 3, "the private upload is skipped");
        assert_eq!(entries[0].video_id, "abc123XYZ_-");
        assert_eq!(entries[0].title, "Virya — Ashes (Official Video)");
        assert_eq!(
            entries[0].published,
            Some(time::macros::datetime!(2026-08-30 18:00 UTC)),
            "release time, not the playlist insertion time"
        );
        assert_eq!(
            entries[0].description.as_deref(),
            Some("Wrote this one in a week.")
        );
        assert_eq!(
            entries[1].description, None,
            "a blank description is absent"
        );
        assert_eq!(
            entries[2].description.as_ref().map(|d| d.chars().count()),
            Some(MAX_DESCRIPTION_CHARS)
        );
        assert!(parse_playlist_items(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn empty_feed_yields_no_entries() {
        assert!(parse_feed("<feed><title>x</title></feed>").is_empty());
    }

    /// The description is the only prose a YouTube feed carries, and dropping
    /// it left the VOICE rule pointing at a title and a URL.
    #[test]
    fn the_bands_own_description_is_kept() {
        let entries = parse_feed(SAMPLE_FEED);
        assert_eq!(
            entries[0].description.as_deref(),
            Some("Wrote this one in a week. Recorded it in two.")
        );
    }

    /// A video with no description is absent, never an empty string — the
    /// voice-sample read excludes a blank body, and an empty string would pass
    /// that filter while teaching the model nothing.
    #[test]
    fn a_missing_description_stays_absent() {
        let entries = parse_feed(SAMPLE_FEED);
        assert_eq!(entries[1].description, None);
    }

    /// The tail of a long description is credits, links and hashtags. Keeping
    /// it would teach the model to write in hashtags, which is the opposite of
    /// what the sample is for.
    #[test]
    fn a_long_description_keeps_its_head_and_never_splits_a_character() {
        // Two-byte characters throughout: a byte-wise truncation at 1,000
        // would land mid-codepoint and panic.
        let long = "ż".repeat(MAX_DESCRIPTION_CHARS + 500);
        let feed = format!(
            "<feed><entry><yt:videoId>x1</yt:videoId><title>t</title>\
             <media:group><media:description>{long}</media:description></media:group>\
             </entry></feed>"
        );
        let entries = parse_feed(&feed);
        let kept = entries[0].description.as_deref().expect("description kept");
        assert_eq!(kept.chars().count(), MAX_DESCRIPTION_CHARS);
        assert!(kept.chars().all(|c| c == 'ż'));
    }

    #[test]
    fn shorts_answer_2xx_full_videos_redirect_and_errors_are_unknown() {
        use reqwest::StatusCode;
        assert_eq!(
            youtube_format_from_status(StatusCode::OK),
            YoutubeVideoFormat::Short
        );
        assert_eq!(
            youtube_format_from_status(StatusCode::SEE_OTHER),
            YoutubeVideoFormat::LongForm
        );
        assert_eq!(
            youtube_format_from_status(StatusCode::FOUND),
            YoutubeVideoFormat::LongForm
        );
        assert_eq!(
            youtube_format_from_status(StatusCode::NOT_FOUND),
            YoutubeVideoFormat::Unknown
        );
        assert_eq!(
            youtube_format_from_status(StatusCode::TOO_MANY_REQUESTS),
            YoutubeVideoFormat::Unknown
        );
    }
}
