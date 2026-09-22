//! Release source sync: watch each connected music platform and register new
//! releases as trusted `release` content sources.
//!
//! A release the band published is the strongest piece of material the content
//! loop can carry — and until now it only arrived if an operator filed it by
//! hand. This worker watches the three places a band's catalogue actually
//! lives:
//!
//! - **Spotify** — the same unauthenticated path the growth-metric sync uses:
//!   the public embed page yields a web-player token, and the pathfinder
//!   GraphQL `queryArtistOverview` response already carries the artist's
//!   discography (albums and singles with real release dates). No app
//!   registration, no stored credential.
//! - **Bandcamp** — the public `/music` grid lists every release with a
//!   stable `data-item-id`; the item page carries the real release date.
//!   Bandcamp has no API, and does not need one for this.
//! - **SoundCloud** — the profile page's hydration JSON names the numeric
//!   user id; a `client_id` scraped from SoundCloud's own web assets then
//!   answers `api-v2` for the full public track list. The RSS feed exists
//!   but is opt-in and silently partial, so it is not used.
//!
//! Like the video watcher, only facts the provider carries are recorded:
//! title, link, release date, and whatever prose the band published with it.
//! Nothing invents a story around them.
//!
//! Wake paths: a `growth_metric_sync` NOTIFY (the fanbase_connections trigger
//! fires it when a connection appears or changes) and a periodic sweep for
//! freshness — none of these platforms notify us when a release lands.
//!
//! Crash safety: each upsert is keyed on `(workspace_id, 'release',
//! '{platform}:{external_id}')` via ON CONFLICT, so a re-run is a no-op and
//! a crash mid-cycle is reclaimed by the next one.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, postgres::PgListener};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::watch;
use uuid::Uuid;

/// How often each connection is re-read. An hour is fresh enough for
/// "a new release appeared" without leaning on public endpoints.
const SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Bound on connections read per cycle, per platform — one workspace should
/// never have many.
const MAX_CONNECTIONS_PER_CYCLE: i64 = 10;
/// Bound on items read per connection — a catalogue page lists everything
/// the artist ever put out; anything older was already captured.
const MAX_ITEMS_PER_CONNECTION: usize = 50;
/// Bound on Bandcamp item-page fetches per connection per cycle — only new
/// items need their release date, so this is the discovery burst limit.
const MAX_BANDCAMP_DETAIL_FETCHES: usize = 10;
/// HTTP timeout for every fetch in this worker.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// A release stays shareable for 90 days — longer than the 45-day event
/// window because a release is a slow arc, not a night.
const SOURCE_LIFETIME_DAYS: i64 = 90;
/// The longest provider-supplied description kept as a voice sample —
/// same bound the video watcher applies to YouTube descriptions.
const MAX_DESCRIPTION_CHARS: usize = 1_000;
/// Column limit on `content_sources.title` is 240; truncate hard.
const MAX_TITLE_CHARS: usize = 230;
const USER_AGENT: &str = "CrowdRelay/1.0 (release source sync)";

#[derive(Debug, Error)]
pub enum ReleaseSourceSyncError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

/// One release as any platform reports it — the facts only. Public so the
/// shipped upsert is exercisable from the postgres suite — a placeholder
/// drift in that query fails only against a real schema.
#[derive(Debug)]
pub struct ReleaseEntry {
    /// Stable platform identifier — becomes `{platform}:{id}` source_key.
    pub external_id: String,
    pub title: String,
    /// Canonical listen/buy link — what the artifact cites.
    pub url: String,
    pub released_at: Option<OffsetDateTime>,
    /// Prose the band published with it (Bandcamp liner notes, SoundCloud
    /// description). Absent stays absent, never an empty string.
    pub description: Option<String>,
    /// What the platform calls it — "album", "single", "track", "ep".
    pub release_type: Option<String>,
}

#[derive(Clone)]
pub struct ReleaseSourceSyncWorker {
    pool: PgPool,
    http_client: reqwest::Client,
    workspace_id: Uuid,
}

impl ReleaseSourceSyncWorker {
    pub fn new(pool: PgPool, workspace_id: Uuid) -> Result<Self, ReleaseSourceSyncError> {
        let http_client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT.min(Duration::from_secs(10)))
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(ReleaseSourceSyncError::ClientBuild)?;
        Ok(Self {
            pool,
            http_client,
            workspace_id,
        })
    }

    /// Main loop: initial sweep on startup, then wake on NOTIFY (a connection
    /// appeared or changed) or on the freshness interval.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), ReleaseSourceSyncError> {
        tracing::info!("release source sync worker started");

        // Same channel the fanbase_connections trigger notifies — a new
        // connection should be read immediately, not in an hour.
        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(ReleaseSourceSyncError::Database)?;
        listener
            .listen("growth_metric_sync")
            .await
            .map_err(ReleaseSourceSyncError::Database)?;

        self.sync_cycle().await;

        let mut tick =
            tokio::time::interval_at(tokio::time::Instant::now() + SYNC_INTERVAL, SYNC_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("release source sync worker shutting down");
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

    /// One cycle: every connected music platform's latest releases become
    /// content sources. Per-connection failures are logged, not propagated —
    /// one unreachable platform must not starve the others.
    async fn sync_cycle(&self) {
        for platform in ["spotify", "bandcamp", "soundcloud"] {
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
                    tracing::error!(error = %error, platform, "release source sync: could not list connections");
                    continue;
                }
            };

            for (connection_id, account_id) in connections {
                let result = match platform {
                    "spotify" => self.sync_spotify(&account_id).await,
                    "bandcamp" => self.sync_bandcamp(&account_id).await,
                    "soundcloud" => self.sync_soundcloud(&account_id).await,
                    _ => Ok(()),
                };
                if let Err(error) = result {
                    tracing::warn!(
                        connection_id = %connection_id,
                        platform,
                        account_id = %account_id,
                        error = %error,
                        "release source sync: connection sweep failed"
                    );
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Spotify — embed token → pathfinder queryArtistOverview → discography
    // ------------------------------------------------------------------

    async fn sync_spotify(&self, artist_id: &str) -> Result<(), String> {
        let embed_url = format!("https://open.spotify.com/embed/artist/{artist_id}");
        let html = self
            .http_client
            .get(&embed_url)
            .send()
            .await
            .map_err(|e| format!("embed fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("embed page returned {e}"))?
            .text()
            .await
            .map_err(|e| format!("embed body read failed: {e}"))?;
        let token = extract_spotify_embed_token(&html)
            .ok_or_else(|| "no access token in Spotify embed page".to_owned())?;

        let variables = format!(
            r#"{{"uri":"spotify:artist:{artist_id}","locale":"","includePrerelease":false}}"#
        );
        let extensions = r#"{"persistedQuery":{"version":1,"sha256Hash":"d66221ea13998b2f81883c5187d174c8646e4041d67f5b1e103bc262d447e3a0"}}"#;
        let graphql_url = format!(
            "https://api-partner.spotify.com/pathfinder/v1/query?operationName=queryArtistOverview&variables={}&extensions={}",
            urlencode(&variables),
            urlencode(extensions),
        );
        let body: SpotifyArtistOverview = self
            .http_client
            .get(&graphql_url)
            .bearer_auth(&token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("pathfinder fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("pathfinder returned {e}"))?
            .json()
            .await
            .map_err(|e| format!("pathfinder body parse failed: {e}"))?;

        let mut entries = Vec::new();
        for shelf in [
            &body.data.artist.discography.albums,
            &body.data.artist.discography.singles,
        ]
        .into_iter()
        .flatten()
        {
            for group in &shelf.items {
                for release in &group.releases.items {
                    let Some(entry) = release_entry(release) else {
                        continue;
                    };
                    entries.push(entry);
                }
            }
        }
        for entry in entries.into_iter().take(MAX_ITEMS_PER_CONNECTION) {
            self.upsert_release("spotify", &entry).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Bandcamp — /music grid → item pages for the real release date
    // ------------------------------------------------------------------

    async fn sync_bandcamp(&self, subdomain: &str) -> Result<(), String> {
        let url = format!("https://{subdomain}.bandcamp.com/music");
        let html = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("music page fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("music page returned {e}"))?
            .text()
            .await
            .map_err(|e| format!("music page read failed: {e}"))?;

        let base = format!("https://{subdomain}.bandcamp.com");
        let mut detail_fetches = 0usize;
        for mut item in parse_bandcamp_grid(&html, &base)
            .into_iter()
            .take(MAX_ITEMS_PER_CONNECTION)
        {
            let source_key = format!("bandcamp:{}", item.external_id);
            if self.source_complete(&source_key).await? {
                continue;
            }
            // Only a new item earns its own page fetch — the grid carries no
            // release date, and the item page's datePublished is the real one.
            if detail_fetches < MAX_BANDCAMP_DETAIL_FETCHES {
                detail_fetches += 1;
                if let Some((released_at, description)) = self.bandcamp_item_detail(&item.url).await
                {
                    item.released_at = released_at;
                    item.description = description;
                }
            }
            self.upsert_release("bandcamp", &item).await?;
        }
        Ok(())
    }

    /// One Bandcamp release page: `datePublished` for the real date and the
    /// `about` block for the band's own liner notes — the same voice-sample
    /// role the video watcher's description plays.
    async fn bandcamp_item_detail(
        &self,
        url: &str,
    ) -> Option<(Option<OffsetDateTime>, Option<String>)> {
        let html = self
            .http_client
            .get(url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .text()
            .await
            .ok()?;
        let released_at =
            extract_meta_content(&html, "datePublished").and_then(|s| parse_bandcamp_date(&s));
        let description = extract_named_section(&html, "about")
            .map(|text| truncate_chars(&text, MAX_DESCRIPTION_CHARS));
        Some((released_at, description))
    }

    // ------------------------------------------------------------------
    // SoundCloud — hydration user id → scraped client_id → api-v2 tracks
    // ------------------------------------------------------------------

    async fn sync_soundcloud(&self, permalink: &str) -> Result<(), String> {
        let profile_url = format!("https://soundcloud.com/{permalink}");
        let html = self
            .http_client
            .get(&profile_url)
            .send()
            .await
            .map_err(|e| format!("profile fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("profile returned {e}"))?
            .text()
            .await
            .map_err(|e| format!("profile read failed: {e}"))?;
        let user = extract_soundcloud_user(&html)
            .ok_or_else(|| "no user in SoundCloud hydration data".to_owned())?;

        let client_id = self.soundcloud_client_id().await?;
        let tracks_url = format!(
            "https://api-v2.soundcloud.com/users/{}/tracks?client_id={}&limit={}",
            user.id, client_id, MAX_ITEMS_PER_CONNECTION
        );
        let page: SoundcloudTracksPage = self
            .http_client
            .get(&tracks_url)
            .send()
            .await
            .map_err(|e| format!("tracks fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("tracks api returned {e}"))?
            .json()
            .await
            .map_err(|e| format!("tracks body parse failed: {e}"))?;

        for track in &page.collection {
            if track.kind.as_deref() != Some("track") {
                continue;
            }
            let (Some(id), Some(title), Some(permalink)) = (
                track.id,
                track.title.as_deref(),
                track.permalink_url.as_deref(),
            ) else {
                continue;
            };
            let entry = ReleaseEntry {
                external_id: id.to_string(),
                title: title.to_owned(),
                url: permalink.to_owned(),
                released_at: track
                    .display_date
                    .as_deref()
                    .or(track.created_at.as_deref())
                    .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok()),
                description: track
                    .description
                    .as_deref()
                    .map(|d| truncate_chars(d.trim(), MAX_DESCRIPTION_CHARS))
                    .filter(|d| !d.is_empty()),
                release_type: Some("track".to_owned()),
            };
            self.upsert_release("soundcloud", &entry).await?;
        }
        Ok(())
    }

    /// SoundCloud's public api-v2 needs a client_id the web app embeds in its
    /// bundles. Scraped per call — the id rotates with deploys and caching it
    /// would turn a stale key into a permanent failure mode.
    async fn soundcloud_client_id(&self) -> Result<String, String> {
        let html = self
            .http_client
            .get("https://soundcloud.com/discover")
            .send()
            .await
            .map_err(|e| format!("discover page fetch failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("discover page returned {e}"))?
            .text()
            .await
            .map_err(|e| format!("discover page read failed: {e}"))?;
        let scripts = extract_script_sources(&html);
        // The client_id lives in the app bundles; probe from the tail where
        // the main bundles sit, not the shared library chunks at the head.
        for src in scripts.iter().rev().take(6) {
            let Ok(js) = self
                .http_client
                .get(src)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            else {
                continue;
            };
            let Ok(body) = js.text().await else {
                continue;
            };
            if let Some(id) = extract_soundcloud_client_id(&body) {
                return Ok(id);
            }
        }
        Err("no client_id in SoundCloud web assets".to_owned())
    }

    // ------------------------------------------------------------------
    // Shared upsert — same shape as the video watcher
    // ------------------------------------------------------------------

    /// Whether a `{platform}:{id}` release row already carries the provider's
    /// own release date. A row filed without one (its detail fetch failed
    /// under a cap or a blip) is not "done" — the next run retries the
    /// detail page rather than keep a sync-time guess as the fact.
    async fn source_complete(&self, source_key: &str) -> Result<bool, String> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM content_sources
                WHERE workspace_id = $1
                  AND source_kind = 'release'
                  AND source_key = $2
                  AND (metadata->>'released_at' IS NOT NULL
                       OR metadata->>'release_date' IS NOT NULL)
            )
            "#,
        )
        .bind(self.workspace_id)
        .bind(source_key)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("source exists check: {e}"))
    }

    /// Idempotent upsert keyed on `{platform}:{external_id}`. A fact change
    /// bumps the version and records history; an unchanged row is a no-op.
    pub async fn upsert_release(&self, platform: &str, entry: &ReleaseEntry) -> Result<(), String> {
        let source_key = format!("{platform}:{}", entry.external_id);
        let title: String = truncate_chars(entry.title.trim(), MAX_TITLE_CHARS);
        if title.is_empty() {
            return Ok(());
        }
        let metadata = json!({
            "url": entry.url,
            "platform": platform,
            "release_type": entry.release_type,
            "released_at": entry.released_at.map(|t| t.unix_timestamp()),
            "origin": format!("{platform}_sync"),
            "body": entry.description,
        });

        let mut tx = self.pool.begin().await.map_err(|e| format!("begin: {e}"))?;
        let upserted: Option<(Uuid, i64)> = sqlx::query_as(
            r#"
            INSERT INTO content_sources (
                id, workspace_id, source_kind, source_key, title,
                occurred_at, expires_at, metadata
            ) VALUES (
                $3, $1, 'release', $2, $4,
                COALESCE($5, now()),
                COALESCE($5, now()) + make_interval(days => $7),
                $6
            )
            ON CONFLICT (workspace_id, source_kind, source_key) DO UPDATE SET
                title = EXCLUDED.title,
                -- The provider's own date wins; a sweep without one keeps the
                -- stored anchor rather than restamping "now" every hour.
                occurred_at = COALESCE($5, content_sources.occurred_at),
                expires_at = GREATEST(
                    content_sources.expires_at,
                    COALESCE($5, content_sources.occurred_at) + make_interval(days => $7)
                ),
                -- The announce endpoint writes the same `spotify:{id}` key with
                -- richer fields (listen_url, image, track count). Merge so a
                -- sweep cannot strip them — shared keys take the fresh write.
                metadata = content_sources.metadata || EXCLUDED.metadata,
                version = content_sources.version + 1
            WHERE content_sources.title IS DISTINCT FROM EXCLUDED.title
               OR content_sources.occurred_at IS DISTINCT FROM EXCLUDED.occurred_at
               OR content_sources.expires_at IS DISTINCT FROM EXCLUDED.expires_at
               OR content_sources.metadata IS DISTINCT FROM EXCLUDED.metadata
            RETURNING id, version
            "#,
        )
        .bind(self.workspace_id)
        .bind(&source_key)
        .bind(Uuid::now_v7())
        .bind(&title)
        .bind(entry.released_at)
        .bind(&metadata)
        .bind(SOURCE_LIFETIME_DAYS as i32)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("upsert: {e}"))?;

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
// Parsers — pure functions over provider payloads
// ----------------------------------------------------------------------

/// The `__NEXT_DATA__` script in a Spotify embed page carries the web-player
/// token the pathfinder API accepts.
fn extract_spotify_embed_token(html: &str) -> Option<String> {
    let marker = "\"accessToken\":\"";
    let start = html.find(marker)? + marker.len();
    let rest = html.get(start..)?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_string())
}

/// Minimal URL-encoder for GraphQL query parameters — the pathfinder API
/// expects variables and extensions URL-encoded in the query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

/// The slice of `queryArtistOverview` this worker reads — the discography
/// shelves ride the same persisted query the metric sync already calls.
#[derive(Debug, Deserialize)]
struct SpotifyArtistOverview {
    data: SpotifyOverviewData,
}
#[derive(Debug, Deserialize)]
struct SpotifyOverviewData {
    artist: SpotifyArtist,
}
#[derive(Debug, Deserialize)]
struct SpotifyArtist {
    discography: SpotifyDiscography,
}
#[derive(Debug, Deserialize)]
struct SpotifyDiscography {
    albums: Option<SpotifyShelf>,
    singles: Option<SpotifyShelf>,
}
#[derive(Debug, Deserialize)]
struct SpotifyShelf {
    items: Vec<SpotifyReleaseGroup>,
}
#[derive(Debug, Deserialize)]
struct SpotifyReleaseGroup {
    releases: SpotifyReleaseList,
}
#[derive(Debug, Deserialize)]
struct SpotifyReleaseList {
    items: Vec<SpotifyRelease>,
}
#[derive(Debug, Deserialize)]
struct SpotifyRelease {
    id: String,
    uri: Option<String>,
    name: Option<String>,
    #[serde(rename = "type")]
    release_type: Option<String>,
    date: Option<SpotifyReleaseDate>,
}
#[derive(Debug, Deserialize)]
struct SpotifyReleaseDate {
    year: Option<i32>,
    month: Option<i32>,
    day: Option<i32>,
}

fn release_entry(release: &SpotifyRelease) -> Option<ReleaseEntry> {
    let title = release.name.as_deref()?.trim();
    if title.is_empty() {
        return None;
    }
    // The canonical link is the open.spotify URL on the release uri —
    // `spotify:album:{id}` becomes `https://open.spotify.com/album/{id}`.
    let url = release.uri.as_deref().and_then(|uri| {
        uri.strip_prefix("spotify:")
            .map(|rest| format!("https://open.spotify.com/{}", rest.replacen(':', "/", 1)))
    })?;
    let released_at = release.date.as_ref().and_then(|d| {
        let year = d.year?;
        let month = time::Month::try_from(u8::try_from(d.month.unwrap_or(1)).ok()?).ok()?;
        let day = u8::try_from(d.day.unwrap_or(1)).ok()?;
        let date = time::Date::from_calendar_date(year, month, day).ok()?;
        Some(date.midnight().assume_utc())
    });
    Some(ReleaseEntry {
        external_id: release.id.clone(),
        title: title.to_owned(),
        url,
        released_at,
        description: None,
        release_type: release.release_type.as_deref().map(str::to_ascii_lowercase),
    })
}

/// Parses a Bandcamp `/music` grid into items. Each `<li>` carries a stable
/// `data-item-id="album-123"` / `track-456`, the release link and the title.
/// A bounded tag scan is enough — the grid is regular.
fn parse_bandcamp_grid(html: &str, base: &str) -> Vec<ReleaseEntry> {
    let mut entries = Vec::new();
    let mut rest = html;
    // Scan `<li` openings — `data-item-id` precedes the class marker inside
    // the tag, so anchoring on `music-grid-item` would cut it off the block.
    while let Some(pos) = rest.find("<li") {
        let Some(after_open) = rest.get(pos..) else {
            break;
        };
        let Some(li_end) = after_open.find("</li>") else {
            break;
        };
        let Some(block) = after_open.get(..li_end) else {
            break;
        };
        let Some(next) = after_open.get(li_end + "</li>".len()..) else {
            break;
        };
        rest = next;
        if !block.contains("music-grid-item") {
            continue;
        }
        let item_id = extract_attr(block, "data-item-id");
        let href = extract_href(block);
        let title = extract_tag(block, "p")
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        if let (Some(item_id), Some(href), Some(title)) = (item_id, href, title) {
            // data-item-id is "album-2456277107" — keep the platform's own
            // split so the key reads what it is.
            let release_type = item_id
                .split('-')
                .next()
                .filter(|t| matches!(*t, "album" | "track"))
                .map(str::to_owned);
            entries.push(ReleaseEntry {
                external_id: item_id,
                title,
                url: format!("{base}{href}"),
                released_at: None,
                description: None,
                release_type,
            });
        }
    }
    entries
}

/// Reads `content` of the first `<meta ... name|itemprop="key" ...>` in a
/// Bandcamp page — `datePublished` is the one this worker wants.
fn extract_meta_content(html: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let pos = html.find(&needle)?;
    let tag_start = html.get(..pos)?.rfind('<')?;
    let tag_end = html.get(pos..)?.find('>')? + pos;
    let tag = html.get(tag_start..=tag_end)?;
    extract_attr(tag, "content")
}

/// The `about` block in a Bandcamp release page: `<div class="tralbum-about"…>`
/// …older pages use `itemprop="about"`. Plain text only — markup is stripped.
fn extract_named_section(html: &str, name: &str) -> Option<String> {
    for marker in [
        format!("class=\"tralbum-{name}\""),
        format!("itemprop=\"{name}\""),
    ] {
        let Some(pos) = html.find(&marker) else {
            continue;
        };
        // A marker that does not parse cleanly must not sink the fallback —
        // continue rather than `?` so the second form still gets its turn.
        let Some(section) = html
            .get(pos..)
            .and_then(|after| after.find('>').and_then(|tag_end| after.get(tag_end + 1..)))
            .and_then(|rest| rest.find("</div>").and_then(|close| rest.get(..close)))
        else {
            continue;
        };
        let text = strip_tags(section);
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    None
}

/// Bandcamp dates arrive as `datePublished="2025-05-01"` or occasionally a
/// fuller ISO timestamp — take the leading `YYYY-MM-DD`.
fn parse_bandcamp_date(raw: &str) -> Option<OffsetDateTime> {
    let trimmed = raw.trim();
    let date_part = trimmed.get(..10)?;
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let date = time::Date::parse(date_part, &format).ok()?;
    Some(date.midnight().assume_utc())
}

/// The `__sc_hydration` array carries a `{"hydratable":"user","data":{…}}`
/// entry on profile pages — the numeric id is all the tracks call needs.
fn extract_soundcloud_user(html: &str) -> Option<SoundcloudUser> {
    let marker = "__sc_hydration";
    let pos = html.find(marker)?;
    let arr_start = html.get(pos..)?.find('[')? + pos;
    // The array is a single-line JSON literal ending before `</script>`.
    let script_end = html.get(arr_start..)?.find("</script>")? + arr_start;
    let body = html.get(arr_start..script_end)?;
    let body = body.trim().trim_end_matches(';');
    let items: Vec<serde_json::Value> = serde_json::from_str(body).ok()?;
    items.into_iter().find_map(|item| {
        if item.get("hydratable").and_then(|h| h.as_str()) == Some("user") {
            serde_json::from_value(item.get("data")?.clone()).ok()
        } else {
            None
        }
    })
}

#[derive(Debug, Deserialize)]
struct SoundcloudUser {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct SoundcloudTracksPage {
    collection: Vec<SoundcloudTrack>,
}
#[derive(Debug, Deserialize)]
struct SoundcloudTrack {
    id: Option<u64>,
    kind: Option<String>,
    title: Option<String>,
    permalink_url: Option<String>,
    display_date: Option<String>,
    created_at: Option<String>,
    description: Option<String>,
}

/// `<script src="…">` URLs from a SoundCloud page — the client_id lives in
/// one of the app bundles.
fn extract_script_sources(html: &str) -> Vec<String> {
    let mut sources = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("<script") {
        let tag_end = match rest.get(pos..).and_then(|after| after.find('>')) {
            Some(end) => pos + end,
            None => break,
        };
        let Some(tag) = rest.get(pos..=tag_end) else {
            break;
        };
        if let Some(src) = extract_attr(tag, "src")
            && src.starts_with("https://")
        {
            sources.push(src);
        }
        let Some(next) = rest.get(tag_end + 1..) else {
            break;
        };
        rest = next;
    }
    sources
}

/// `client_id:"<id>"` inside a bundle. Google's OAuth client ids also match
/// the shape — they end `.apps.googleusercontent.com`, which the length
/// check rejects.
fn extract_soundcloud_client_id(js: &str) -> Option<String> {
    let mut rest = js;
    while let Some(pos) = rest.find("client_id:\"") {
        let start = pos + "client_id:\"".len();
        let after = rest.get(start..)?;
        let end = after.find('"')?;
        let candidate = after.get(..end)?;
        if candidate.bytes().all(|b| b.is_ascii_alphanumeric())
            && (20..=64).contains(&candidate.len())
        {
            return Some(candidate.to_owned());
        }
        rest = after.get(end..)?;
    }
    None
}

/// First `<tag …>` attribute value in a block.
fn extract_attr(block: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = block.find(&needle)? + needle.len();
    let rest = block.get(start..)?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_owned())
}

/// First `href="…"` in a block.
fn extract_href(block: &str) -> Option<String> {
    extract_attr(block, "href")
}

/// Text of the first `<tag>…</tag>` in a block.
fn extract_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = block.find(&open)?;
    let tag_end = block.get(start..)?.find('>')? + start + 1;
    let rest = block.get(tag_end..)?;
    let end = rest.find(&format!("</{tag}>"))?;
    Some(strip_tags(rest.get(..end)?))
}

/// Remove markup tags from an HTML fragment — Bandcamp's about blocks carry
/// `<br>` and `<a>` that mean nothing to a voice sample.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // Collapse the run of whitespace markup removal leaves behind.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        text.chars().take(max).collect()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotify_overview_releases_become_entries() {
        let body = r#"{
            "data": {"artist": {"discography": {
                "albums": {"items": [{"releases": {"items": [{
                    "id": "5dAAKIVnr96ILc9gxPnRzt",
                    "uri": "spotify:album:5dAAKIVnr96ILc9gxPnRzt",
                    "name": "Echoes Of The Modern Mind",
                    "type": "ALBUM",
                    "date": {"year": 2025, "month": 5, "day": 1}
                }]}}]},
                "singles": {"items": [{"releases": {"items": [{
                    "id": "abc123",
                    "uri": "spotify:album:abc123",
                    "name": "Seed Of Doubt",
                    "type": "SINGLE",
                    "date": {"year": 2025}
                }]}}]}
            }}}
        }"#;
        let parsed: SpotifyArtistOverview = serde_json::from_str(body).unwrap();
        let discography = &parsed.data.artist.discography;
        let album = &discography.albums.as_ref().unwrap().items[0].releases.items[0];
        let entry = release_entry(album).expect("album entry");
        assert_eq!(entry.external_id, "5dAAKIVnr96ILc9gxPnRzt");
        assert_eq!(entry.title, "Echoes Of The Modern Mind");
        assert_eq!(
            entry.url,
            "https://open.spotify.com/album/5dAAKIVnr96ILc9gxPnRzt"
        );
        assert_eq!(entry.release_type.as_deref(), Some("album"));
        assert_eq!(entry.released_at.unwrap().date().year(), 2025);
        // Year-only precision still lands — the first of the year, honestly.
        let single = &discography.singles.as_ref().unwrap().items[0]
            .releases
            .items[0];
        assert!(release_entry(single).unwrap().released_at.is_some());
    }

    #[test]
    fn bandcamp_grid_yields_stable_ids() {
        let html = r#"<ul class="music-grid">
            <li data-item-id="album-2456277107" class="music-grid-item square">
                <a href="/album/echoes-of-the-modern-mind"><p class="title">Echoes Of The Modern Mind</p></a>
            </li>
            <li data-item-id="track-3841206280" class="music-grid-item">
                <a href="/track/seed-of-doubt"><p class="title">Seed Of Doubt</p></a>
            </li>
        </ul>"#;
        let entries = parse_bandcamp_grid(html, "https://virya.bandcamp.com");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].external_id, "album-2456277107");
        assert_eq!(entries[0].title, "Echoes Of The Modern Mind");
        assert_eq!(entries[0].release_type.as_deref(), Some("album"));
        assert_eq!(
            entries[0].url,
            "https://virya.bandcamp.com/album/echoes-of-the-modern-mind"
        );
        assert_eq!(entries[1].release_type.as_deref(), Some("track"));
    }

    #[test]
    fn bandcamp_date_and_about_parse() {
        let html = r#"<meta itemprop="datePublished" content="2025-05-01">
            <div class="tralbum-about">We wrote this in a <b>week</b>.</div>"#;
        assert_eq!(
            extract_meta_content(html, "datePublished").as_deref(),
            Some("2025-05-01")
        );
        assert!(parse_bandcamp_date("2025-05-01").is_some());
        assert_eq!(
            extract_named_section(html, "about").as_deref(),
            Some("We wrote this in a week.")
        );
    }

    #[test]
    fn soundcloud_hydration_finds_user() {
        let html = r#"<script>window.__sc_hydration = [{"hydratable":"user","data":{"id":430558,"permalink":"four-tet"}}];</script>"#;
        let user = extract_soundcloud_user(html).expect("user");
        assert_eq!(user.id, 430558);
    }

    #[test]
    fn soundcloud_client_id_skips_oauth_shaped_values() {
        let js = r#"a={client_id:"984739005367.apps.googleusercontent.com"};b={client_id:"Pb72ranhoyt6gw7hM7TkzUItXlMWSNSo"}"#;
        // The googleusercontent value contains dots — the alnum check rejects
        // it and the scan continues to the real one.
        assert_eq!(
            extract_soundcloud_client_id(js).as_deref(),
            Some("Pb72ranhoyt6gw7hM7TkzUItXlMWSNSo")
        );
    }

    #[test]
    fn strip_tags_collapses_markup_whitespace() {
        assert_eq!(
            strip_tags("a <br> b <a href=\"x\">c</a>"),
            "a  b  c".replace("  ", " ")
        );
        assert_eq!(strip_tags("<p>hi</p>"), "hi");
    }
}
