//! Reactive growth metric sync: YouTube, Spotify, and Reddit (social).
//!
//! Design: reactive, not polling. The worker uses Postgres LISTEN/NOTIFY to
//! wake only when:
//!   1. A new connection is created (trigger fires NOTIFY on the
//!      `growth_metric_sync` channel), or
//!   2. The next scheduled sync time arrives (computed from the latest
//!      recorded point's timestamp — sleep_until, not a ticker).
//!
//! No busy loop. No wake-without-work. When no connections exist, the worker
//! sleeps indefinitely (only wakes on NOTIFY or shutdown).
//!
//! Each sync:
//!   - Finds connections whose latest metric point is older than the sync
//!     interval (or has no points yet — first sight).
//!   - For YouTube: calls the Data API v3 channels endpoint with the stored
//!     API key. No OAuth token needed for public channel statistics.
//!   - For Spotify: uses client credentials flow to get an access token, then
//!     calls the Web API artists endpoint for follower counts.
//!   - For Reddit: calls the public about.json endpoint for subreddit
//!     subscriber counts. No auth needed. Recorded under platform='social'
//!     in the growth metric series (Reddit feeds the "social" coverage bucket).
//!   - Records the point into viryaos_growth_metric_series, declaring the
//!     series on first sight (same pattern as the Bandsintown tracker).
//!
//! Crash safety: each point insert is idempotent via ON CONFLICT DO NOTHING
//! on (workspace_id, series_id, captured_at). A crash mid-sync leaves no
//! partial state — the next wake reclaims the work.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_infra::sensitive_response::{SensitiveResponseKey, decrypt_value, encrypt_value};

mod connection_health;
mod simple_platforms;
use serde::Deserialize;
use sqlx::{FromRow, PgPool, postgres::PgListener};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{Instant, sleep, timeout},
};
use uuid::Uuid;

/// How often to sync each connection's metrics. The series'
/// `expected_interval_hours` is 24, so we sync once per day. The worker wakes
/// sooner if a NOTIFY arrives (new connection).
const SYNC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Fallback sleep when no connections are due: check again in 5 minutes.
/// This is NOT a poll — it's a safety net in case a NOTIFY is missed.
const FALLBACK_SLEEP: Duration = Duration::from_secs(5 * 60);
/// HTTP timeout for provider calls.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum connections to sync per cycle.
const MAX_CONNECTIONS_PER_CYCLE: usize = 10;

const USER_AGENT: &str = "CrowdRelay/1.0 (growth metric sync)";

/// Platforms this worker polls.
///
/// Spelled out rather than computed because `scripts/test_platform_vocabulary_contract.py`
/// reads these literals to check them against the `fanbase_connections` CHECK
/// constraint. The list is not free-hand, though: `synced_platforms_match_the_domain`
/// asserts it equals `Platform::ALL` filtered by `polled_by_growth_metric_sync`,
/// so the enum stays the source of truth and this stays greppable.
///
/// The fanbase_connection platform is the connectable surface; the metric
/// series platform is the coverage bucket. Reddit connections record under
/// 'social' because the MetricPlatform enum has no 'reddit' variant — Reddit
/// feeds the social coverage bucket.
const SYNCED_PLATFORMS: &[&str] = &[
    "tiktok",
    // No "reddit": see `Platform::polled_by_growth_metric_sync`. The poll
    // scraped community sizes through proxies Reddit blocks, and that number
    // is not this artist's audience.
    "spotify",
    "youtube",
    "facebook",
    "instagram",
    "soundcloud",
    "discord",
    "telegram",
    "lastfm",
    "deezer",
    "discogs",
    "bluesky",
    "bandcamp",
    "x",
];

#[derive(Debug, Error)]
pub enum GrowthMetricSyncError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider API error: {0}")]
    ProviderApi(String),
    #[error("no youtube API key configured")]
    NoYoutubeApiKey,
    #[error("no facebook page access token configured")]
    NoFacebookToken,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct GrowthMetricSyncWorker {
    pool: PgPool,
    http_client: reqwest::Client,
    youtube_api_key: Option<String>,
    /// Facebook Page access token for Graph API calls.
    facebook_page_access_token: Option<String>,
    /// Where the agent service lives, and the key used to derive its
    /// per-workspace bearer. Reddit is the only platform here that cannot be
    /// read without a credential somebody else holds.
    agent_service_url: String,
    agent_service_auth_key: Option<String>,
    tiktok_client_key: Option<String>,
    tiktok_client_secret: Option<String>,
    /// Encryption key for decrypting OAuth tokens stored in
    /// `encrypted_access_token` / `encrypted_refresh_token`.
    response_encryption_key: SensitiveResponseKey,
    /// Last.fm API key for artist.getInfo calls.
    lastfm_api_key: Option<String>,
    /// Discogs personal access token for artist stats calls.
    discogs_token: Option<String>,
    operation_timeout: Duration,
}

impl GrowthMetricSyncWorker {
    /// Creates a new worker.
    ///
    /// The worker is always spawned. It used to return `Ok(None)` when no
    /// process-level API key was set, but four platforms need none —
    /// `Platform::syncs_without_process_credential` lists them: Discord reads a
    /// free public API, Telegram carries its bot token on the connection row,
    /// Reddit goes through the proxy pool and Spotify mints a token from the
    /// public embed page. Under the old gate an operator could register a
    /// Discord connection, get a 201, and have it never sync with nothing
    /// logged. Idle cost is one `PgListener` connection: the loop waits on
    /// NOTIFY and does no work until a due connection exists.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        youtube_api_key: Option<String>,
        facebook_page_access_token: Option<String>,
        agent_service_url: String,
        agent_service_auth_key: Option<String>,
        tiktok_client_key: Option<String>,
        tiktok_client_secret: Option<String>,
        lastfm_api_key: Option<String>,
        discogs_token: Option<String>,
        response_encryption_key: SensitiveResponseKey,
        operation_timeout: Duration,
    ) -> Result<Option<Self>, GrowthMetricSyncError> {
        // Report which platforms this process can actually reach, so a
        // connection that will never sync is visible at startup instead of
        // being diagnosed from an absence of metric points weeks later.
        let credentialled = [
            ("youtube", youtube_api_key.is_some()),
            ("facebook/instagram", facebook_page_access_token.is_some()),
            ("tiktok", tiktok_client_key.is_some()),
            ("lastfm", lastfm_api_key.is_some()),
            ("discogs", discogs_token.is_some()),
        ];
        let missing: Vec<&str> = credentialled
            .iter()
            .filter(|(_, present)| !present)
            .map(|(name, _)| *name)
            .collect();
        if !missing.is_empty() {
            tracing::warn!(
                platforms = %missing.join(","),
                "growth metric sync: no credentials for these platforms; \
                 connections to them will fail until the keys are set"
            );
        }
        // The main HTTP client is used for YouTube and Spotify — no proxy.
        // Reddit gets its own per-request clients with proxies.
        let http_client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT.min(Duration::from_secs(10)))
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(GrowthMetricSyncError::ClientBuild)?;
        if agent_service_auth_key.is_none() {
            tracing::warn!(
                "growth metric sync: no agent service key; reddit connections cannot be read"
            );
        }
        Ok(Some(Self {
            pool,
            http_client,
            youtube_api_key,
            facebook_page_access_token,
            agent_service_url,
            agent_service_auth_key,
            tiktok_client_key,
            tiktok_client_secret,
            response_encryption_key,
            lastfm_api_key,
            discogs_token,
            operation_timeout,
        }))
    }

    /// Main loop: reactive. LISTENs on `growth_metric_sync` channel and
    /// sleeps until the next due connection. No ticker.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), GrowthMetricSyncError> {
        tracing::info!("growth metric sync worker started (reactive mode)");

        // PgListener uses its own connection, separate from the pool.
        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(GrowthMetricSyncError::Database)?;
        listener
            .listen("growth_metric_sync")
            .await
            .map_err(GrowthMetricSyncError::Database)?;

        // Initial sync on startup — catches connections that became due
        // while the worker was down.
        self.sync_cycle_interruptibly(&mut shutdown).await;

        loop {
            let next_due = self.next_due_time().await;
            let sleep_duration = next_due
                .map(|instant| instant.saturating_duration_since(Instant::now()))
                .unwrap_or(FALLBACK_SLEEP);

            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("growth metric sync worker shutting down");
                        return Ok(());
                    }
                }
                // NOTIFY from Postgres: a new connection was created.
                _ = listener.recv() => {
                    tracing::debug!("growth_metric_sync NOTIFY received");
                    self.sync_cycle_interruptibly(&mut shutdown).await;
                }
                // Scheduled wake: a connection's next sync time arrived.
                _ = sleep(sleep_duration) => {
                    self.sync_cycle_interruptibly(&mut shutdown).await;
                }
            }
        }
    }

    /// A cycle that stops when the process is asked to.
    ///
    /// `tokio::select!` races branch *futures*, but a branch's handler body runs
    /// after the race — so `sync_cycle().await` in a handler saw no shutdown,
    /// outlived Docker's grace period and died on SIGKILL (137). Racing the
    /// cycle makes every await inside it a cancellation point. Safe to cancel:
    /// each point is its own insert and the next cycle recomputes what is due.
    async fn sync_cycle_interruptibly(&self, shutdown: &mut watch::Receiver<bool>) {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                tracing::info!("growth metric sync cycle abandoned for shutdown");
            }
            () = self.sync_cycle() => {}
        }
    }

    /// One sync cycle: find due connections, fetch metrics, record points.
    async fn sync_cycle(&self) {
        let cycle_timeout = Duration::from_secs(self.operation_timeout.as_secs() * 3);
        let result = timeout(cycle_timeout, async {
            let connections = self.find_due_connections().await?;
            if connections.is_empty() {
                return Ok::<_, GrowthMetricSyncError>(());
            }
            tracing::info!(
                connections = connections.len(),
                "growth metric sync cycle: syncing due connections"
            );
            for conn in connections {
                // Per-connection timeout prevents one slow provider
                // (e.g. Reddit proxy testing) from starving the cycle.
                let per_conn = Duration::from_secs(20);
                let result = timeout(per_conn, self.sync_connection(&conn)).await;
                // Outcome goes on the connection, not only in the log: five
                // of production's connections have failed every cycle since
                // creation while all reporting `connected`. See
                // connection_health.rs.
                self.record_sync_outcome(&conn, &result).await;
            }
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::error!(error = %error, "growth metric sync cycle error");
            }
            Err(_) => {
                tracing::warn!("growth metric sync cycle timed out");
            }
        }
    }

    /// Finds connections whose latest metric point is older than SYNC_INTERVAL
    /// (or has no points yet). Returns at most MAX_CONNECTIONS_PER_CYCLE.
    async fn find_due_connections(&self) -> Result<Vec<DueConnection>, GrowthMetricSyncError> {
        let rows = sqlx::query_as::<_, DueConnectionRow>(
            r#"
            SELECT
                fc.id, fc.workspace_id, fc.platform, fc.provider_account_id,
                fc.external_account_ref
            FROM fanbase_connections fc
            WHERE fc.status = 'connected'
              AND fc.platform = ANY($1)
              AND fc.provider_account_id IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1
                  FROM viryaos_growth_metric_points p
                  JOIN viryaos_growth_metric_series s ON s.id = p.series_id
                  WHERE s.workspace_id = fc.workspace_id
                    AND s.subject_kind = 'fanbase_connection'
                    AND s.subject_id = fc.id
                    AND p.captured_at > now() - ($2::bigint * interval '1 second')
              )
            ORDER BY CASE fc.platform
                        WHEN 'youtube' THEN 1
                        WHEN 'spotify' THEN 2
                        WHEN 'tiktok' THEN 3
                        WHEN 'facebook' THEN 4
                        WHEN 'instagram' THEN 5
                        WHEN 'soundcloud' THEN 6
                        WHEN 'reddit' THEN 7
                        WHEN 'discord' THEN 8
                        WHEN 'telegram' THEN 9
                        WHEN 'lastfm' THEN 10
                        WHEN 'deezer' THEN 11
                        WHEN 'discogs' THEN 12
                        WHEN 'bluesky' THEN 13
                        WHEN 'bandcamp' THEN 14
                        ELSE 15
                     END,
                     fc.created_at
            LIMIT $3
            "#,
        )
        .bind(SYNCED_PLATFORMS)
        .bind(SYNC_INTERVAL.as_secs() as i64)
        .bind(MAX_CONNECTIONS_PER_CYCLE as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| DueConnection {
                id: row.id,
                workspace_id: row.workspace_id,
                platform: row.platform,
                provider_account_id: row.provider_account_id,
                external_account_ref: row.external_account_ref,
            })
            .collect())
    }

    /// Computes the earliest next-due time across all connections. Returns
    /// None if no connections exist (sleep until NOTIFY).
    async fn next_due_time(&self) -> Option<Instant> {
        // Find the oldest "last sync" time across all connections. The next
        // due time is that + SYNC_INTERVAL. If no points exist yet, the
        // connection is due now.
        let next: Option<time::OffsetDateTime> = sqlx::query_scalar(
            r#"
            SELECT MIN(p.captured_at)
            FROM fanbase_connections fc
            JOIN viryaos_growth_metric_series s
              ON s.workspace_id = fc.workspace_id
             AND s.subject_kind = 'fanbase_connection'
             AND s.subject_id = fc.id
            JOIN LATERAL (
                SELECT captured_at
                FROM viryaos_growth_metric_points
                WHERE series_id = s.id
                ORDER BY captured_at DESC
                LIMIT 1
            ) p ON true
            WHERE fc.status = 'connected'
              AND fc.platform = ANY($1)
              AND fc.provider_account_id IS NOT NULL
            "#,
        )
        .bind(SYNCED_PLATFORMS)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();

        // Also check if any connection has no points yet (due immediately).
        let has_unsynced: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM fanbase_connections fc
                WHERE fc.status = 'connected'
                  AND fc.platform = ANY($1)
                  AND fc.provider_account_id IS NOT NULL
                  AND NOT EXISTS (
                      SELECT 1
                      FROM viryaos_growth_metric_points p
                      JOIN viryaos_growth_metric_series s ON s.id = p.series_id
                      WHERE s.workspace_id = fc.workspace_id
                        AND s.subject_kind = 'fanbase_connection'
                        AND s.subject_id = fc.id
                  )
            )
            "#,
        )
        .bind(SYNCED_PLATFORMS)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(false);

        if has_unsynced {
            return Some(Instant::now());
        }

        next.map(|captured_at| {
            let elapsed = OffsetDateTime::now_utc() - captured_at;
            let remaining = SYNC_INTERVAL
                .saturating_sub(Duration::from_secs(elapsed.whole_seconds().max(0) as u64));
            Instant::now() + remaining
        })
    }

    /// Syncs a single connection: fetches the metric from the provider and
    /// records the point.
    async fn sync_connection(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        match conn.platform.as_str() {
            "youtube" => self.sync_youtube(conn).await,
            "spotify" => self.sync_spotify(conn).await,
            "reddit" => self.sync_reddit(conn).await,
            "facebook" => self.sync_facebook(conn).await,
            "instagram" => self.sync_instagram(conn).await,
            "soundcloud" => self.sync_soundcloud(conn).await,
            "tiktok" => self.sync_tiktok(conn).await,
            "discord" => self.sync_discord(conn).await,
            "telegram" => self.sync_telegram(conn).await,
            "lastfm" => self.sync_lastfm(conn).await,
            "deezer" => self.sync_deezer(conn).await,
            "discogs" => self.sync_discogs(conn).await,
            "bluesky" => self.sync_bluesky(conn).await,
            "bandcamp" => self.sync_bandcamp(conn).await,
            "x" => self.sync_x(conn).await,
            // The lease query filters on SYNCED_PLATFORMS, so reaching this arm
            // means that list and this match disagree. Returning Ok would mark
            // the connection synced and record nothing — the failure would look
            // like a platform that simply never moves. Fail loudly instead.
            other => Err(GrowthMetricSyncError::ProviderApi(format!(
                "connection platform '{other}' is leased for sync but has no sync arm"
            ))),
        }
    }

    /// YouTube: fetch subscriber count via Data API v3 (API key, no OAuth).
    async fn sync_youtube(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let api_key = self
            .youtube_api_key
            .as_ref()
            .ok_or(GrowthMetricSyncError::NoYoutubeApiKey)?;
        let channel_id = &conn.provider_account_id;

        let url = format!(
            "https://www.googleapis.com/youtube/v3/channels?part=statistics&id={channel_id}&key={api_key}"
        );
        // The API key is a query parameter, so it is inside the request URL
        // and reqwest's Display would carry it into the log. Strip the URL
        // off every error out of this call — same pattern as the Telegram
        // and Last.fm syncs in simple_platforms.rs.
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "YouTube API returned HTTP {}",
                response.status()
            )));
        }
        let body: YoutubeChannelsResponse = response.json().await?;
        let subscriber_count = body
            .items
            .first()
            .and_then(|item| item.statistics.subscriber_count.as_ref())
            .and_then(normalize_count)
            .ok_or(GrowthMetricSyncError::ProviderApi(
                "YouTube API returned no subscriber count".to_owned(),
            ))?;

        let display_name = "YouTube channel";
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "youtube",
            "subscribers",
            &format!("YouTube subscribers — {display_name}"),
            subscriber_count,
            OffsetDateTime::now_utc(),
        )
        .await?;

        tracing::info!(
            connection_id = %conn.id,
            channel_id = %channel_id,
            subscribers = subscriber_count,
            "youtube subscriber count recorded"
        );
        Ok(())
    }

    /// Spotify: fetch artist follower count via the internal partner API.
    /// Uses Spotify's public embed page to obtain a web player token, then
    /// calls the pathfinder GraphQL API for artist stats. No API key, no
    /// app registration, no Extended Quota needed.
    async fn sync_spotify(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let artist_id = &conn.provider_account_id;

        // Spotify deprecated the `followers` field in the public Web API
        // (client credentials flow) in February 2026. The field is omitted
        // from all responses for Development Mode apps. Extended Quota Mode
        // (where it still works) requires a registered organization with
        // 250k+ MAUs — not feasible.
        //
        // Instead, we use Spotify's internal partner API — the same one the
        // web player at open.spotify.com uses. The approach:
        //   1. Fetch the embed page HTML for the artist
        //   2. Extract the public web player access token from the HTML
        //   3. Call the pathfinder GraphQL API (queryArtistOverview) with
        //      that token to get stats.followers and stats.monthlyListeners
        //
        // No API key, no app registration, no Extended Quota needed. The
        // embed token is a public token that Spotify gives to anyone
        // loading the embed page.

        // Step 1: Fetch the embed page and extract the access token.
        let embed_url = format!("https://open.spotify.com/embed/artist/{artist_id}");
        let embed_response = self.http_client.get(&embed_url).send().await?;
        if !embed_response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Spotify embed page returned HTTP {} for artist {artist_id}",
                embed_response.status()
            )));
        }
        let html = embed_response.text().await?;
        let token = extract_spotify_embed_token(&html).ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(
                "could not extract access token from Spotify embed page".to_string(),
            )
        })?;

        // Step 2: Call the pathfinder GraphQL API for artist stats.
        let variables = format!(
            r#"{{"uri":"spotify:artist:{artist_id}","locale":"","includePrerelease":false}}"#
        );
        let extensions = r#"{"persistedQuery":{"version":1,"sha256Hash":"d66221ea13998b2f81883c5187d174c8646e4041d67f5b1e103bc262d447e3a0"}}"#;
        let graphql_url = format!(
            "https://api-partner.spotify.com/pathfinder/v1/query?operationName=queryArtistOverview&variables={}&extensions={}",
            urlencode(&variables),
            urlencode(extensions),
        );
        let response = self
            .http_client
            .get(&graphql_url)
            .bearer_auth(&token)
            .header("Accept", "application/json")
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Spotify pathfinder API returned HTTP {} for artist {artist_id}",
                response.status()
            )));
        }
        let body: SpotifyPartnerArtistResponse = response.json().await?;
        let stats = body.data.artist.stats.ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(
                "Spotify pathfinder response missing stats field".to_string(),
            )
        })?;
        let follower_count = stats.followers;
        let display_name = body.data.artist.profile.name;

        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "spotify",
            "followers",
            &format!("Spotify followers — {display_name}"),
            follower_count,
            OffsetDateTime::now_utc(),
        )
        .await?;

        tracing::info!(
            connection_id = %conn.id,
            artist_id = %artist_id,
            artist = %display_name,
            followers = follower_count,
            monthly_listeners = stats.monthly_listeners,
            "spotify follower count recorded via partner API"
        );
        Ok(())
    }

    /// Facebook: fetch Page fan_count and followers_count via Graph API.
    /// Uses a Page access token (no App Review needed for owned pages).
    /// Recorded under platform='facebook' in the growth metric series.
    async fn sync_facebook(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let token = self
            .facebook_page_access_token
            .as_ref()
            .ok_or(GrowthMetricSyncError::NoFacebookToken)?;
        let page_id = &conn.provider_account_id;

        let url = format!(
            "https://graph.facebook.com/v21.0/{page_id}?fields=name,fan_count,followers_count&access_token={token}"
        );
        // The Page access token is a query parameter, so it is inside the
        // request URL. Strip the URL off transport errors so the token
        // cannot reach the log — same pattern as YouTube above.
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Facebook Graph API returned HTTP {} for page {page_id}",
                response.status()
            )));
        }
        let body: FacebookPageResponse = response.json().await?;
        let follower_count = body.followers_count.unwrap_or(body.fan_count);

        let display_name = body.name.as_deref().unwrap_or("Facebook Page");
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "facebook",
            "followers",
            &format!("Facebook followers — {display_name}"),
            follower_count,
            OffsetDateTime::now_utc(),
        )
        .await?;

        tracing::info!(
            connection_id = %conn.id,
            page_id = %page_id,
            followers = follower_count,
            "facebook page follower count recorded"
        );
        Ok(())
    }

    /// Instagram: fetch IG professional account follower count via the
    /// Graph API. Uses the same Facebook Page access token — the IG
    /// Business account is linked to the Facebook Page, so the Page
    /// token grants access to the IG account's data. No separate
    /// Instagram login flow needed.
    /// Recorded under platform='instagram' in the growth metric series.
    async fn sync_instagram(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let token = self
            .facebook_page_access_token
            .as_ref()
            .ok_or(GrowthMetricSyncError::NoFacebookToken)?;
        let ig_user_id = &conn.provider_account_id;

        let url = format!(
            "https://graph.facebook.com/v21.0/{ig_user_id}?fields=username,followers_count&access_token={token}"
        );
        // The Page access token is a query parameter — strip the URL off
        // transport errors so the token cannot reach the log.
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|error| GrowthMetricSyncError::Http(error.without_url()))?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Instagram Graph API returned HTTP {} for ig_user {ig_user_id}",
                response.status()
            )));
        }
        let body: InstagramUserResponse = response.json().await?;
        let follower_count = body.followers_count;
        let display_name = body.username.as_deref().unwrap_or("Instagram");

        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "instagram",
            "followers",
            &format!("Instagram followers — {display_name}"),
            follower_count,
            OffsetDateTime::now_utc(),
        )
        .await?;

        tracing::info!(
            connection_id = %conn.id,
            ig_user_id = %ig_user_id,
            followers = follower_count,
            "instagram follower count recorded"
        );
        Ok(())
    }

    /// SoundCloud: fetch artist follower count from the public artist page.
    /// SoundCloud embeds user data as hydration JSON in the page HTML. No
    /// API key or app registration needed — same approach as Spotify.
    async fn sync_soundcloud(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let permalink = &conn.provider_account_id;
        let url = format!("https://soundcloud.com/{permalink}");
        let response = self.http_client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "SoundCloud page returned HTTP {} for {permalink}",
                response.status()
            )));
        }
        let html = response.text().await?;
        let user_data = extract_soundcloud_user_data(&html).ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(
                "could not extract user data from SoundCloud page".to_string(),
            )
        })?;
        let follower_count = user_data.followers_count;
        let display_name = user_data.username;

        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "soundcloud",
            "followers",
            &format!("SoundCloud followers — {display_name}"),
            follower_count,
            OffsetDateTime::now_utc(),
        )
        .await?;

        tracing::info!(
            connection_id = %conn.id,
            permalink = %permalink,
            artist = %display_name,
            followers = follower_count,
            "soundcloud follower count recorded"
        );
        Ok(())
    }

    /// X (Twitter): fetch profile follower count from the public profile page.
    /// X server-renders profile data as schema.org JSON-LD (a `ProfilePage`
    /// whose `mainEntity` is a `Person`) into the HTML. No API key or app
    /// registration needed — same scraping approach as SoundCloud and Spotify.
    async fn sync_x(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let handle = &conn.provider_account_id;
        let url = format!("https://x.com/{handle}");
        let response = self.http_client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "X profile page returned HTTP {} for @{handle}",
                response.status()
            )));
        }
        let html = response.text().await?;
        let profile = extract_x_profile_data(&html).ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(format!(
                "could not extract profile data from X page for @{handle}"
            ))
        })?;
        let follower_count = profile.follower_count();
        let display_name = profile.name;

        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "x",
            "followers",
            &format!("X followers — {display_name} (@{handle})"),
            follower_count,
            OffsetDateTime::now_utc(),
        )
        .await?;

        tracing::info!(
            connection_id = %conn.id,
            handle = %handle,
            name = %display_name,
            followers = follower_count,
            "x follower count recorded"
        );
        Ok(())
    }

    /// TikTok: fetch creator follower count via the Display API
    /// /v2/user/info/ endpoint. Requires OAuth tokens stored encrypted in
    /// `encrypted_access_token` / `encrypted_refresh_token`. Refreshes the
    /// access token if expired. On refresh failure, marks the connection
    /// as `expired` and returns an error — never silently falls back to
    /// old expired credentials.
    async fn sync_tiktok(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let client_key = self.tiktok_client_key.as_ref().ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi("no TikTok client key configured".to_string())
        })?;
        let client_secret = self.tiktok_client_secret.as_ref().ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi("no TikTok client secret configured".to_string())
        })?;

        let open_id = &conn.provider_account_id;

        // Read encrypted tokens from the connection.
        let row: (Option<String>, Option<String>, Option<OffsetDateTime>) = sqlx::query_as(
            r#"SELECT encrypted_access_token, encrypted_refresh_token, token_expires_at
               FROM fanbase_connections WHERE id = $1"#,
        )
        .bind(conn.id)
        .fetch_one(&self.pool)
        .await?;

        let encrypted_access = row.0.ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(
                "TikTok connection missing encrypted_access_token".to_string(),
            )
        })?;
        let encrypted_refresh = row.1.ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(
                "TikTok connection missing encrypted_refresh_token".to_string(),
            )
        })?;
        let token_expires_at = row.2.ok_or_else(|| {
            GrowthMetricSyncError::ProviderApi(
                "TikTok connection missing token_expires_at".to_string(),
            )
        })?;

        // Decrypt tokens at point of use.
        let aad = tiktok_oauth_aad(conn.workspace_id, open_id);
        let access_bytes = URL_SAFE_NO_PAD.decode(&encrypted_access).map_err(|_| {
            GrowthMetricSyncError::ProviderApi(
                "TikTok access token is not valid base64".to_string(),
            )
        })?;
        let refresh_bytes = URL_SAFE_NO_PAD.decode(&encrypted_refresh).map_err(|_| {
            GrowthMetricSyncError::ProviderApi(
                "TikTok refresh token is not valid base64".to_string(),
            )
        })?;
        let mut access_token = String::from_utf8(
            decrypt_value(&access_bytes, &self.response_encryption_key, &aad).map_err(|e| {
                GrowthMetricSyncError::ProviderApi(format!(
                    "TikTok access token decryption failed: {e}"
                ))
            })?,
        )
        .map_err(|_| {
            GrowthMetricSyncError::ProviderApi("TikTok access token is not valid UTF-8".to_string())
        })?;
        let refresh_token = String::from_utf8(
            decrypt_value(&refresh_bytes, &self.response_encryption_key, &aad).map_err(|e| {
                GrowthMetricSyncError::ProviderApi(format!(
                    "TikTok refresh token decryption failed: {e}"
                ))
            })?,
        )
        .map_err(|_| {
            GrowthMetricSyncError::ProviderApi(
                "TikTok refresh token is not valid UTF-8".to_string(),
            )
        })?;

        // Refresh the access token if it's expired (or about to expire).
        let now = OffsetDateTime::now_utc();
        if token_expires_at <= now + time::Duration::seconds(60) {
            tracing::info!(
                connection_id = %conn.id,
                open_id = open_id,
                "TikTok access token expired, refreshing"
            );
            let refresh_response = self
                .http_client
                .post("https://open.tiktokapis.com/v2/oauth/token/")
                .form(&[
                    ("client_key", client_key.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("refresh_token", refresh_token.as_str()),
                    ("grant_type", "refresh_token"),
                ])
                .send()
                .await?;

            if !refresh_response.status().is_success() {
                // Mark the connection as expired — the operator must re-auth.
                tracing::warn!(
                    connection_id = %conn.id,
                    open_id = open_id,
                    status = refresh_response.status().as_u16(),
                    "TikTok token refresh failed, marking connection as expired"
                );
                let _ = sqlx::query(
                    "UPDATE fanbase_connections SET status = 'expired', updated_at = now() WHERE id = $1",
                )
                .bind(conn.id)
                .execute(&self.pool)
                .await;
                return Err(GrowthMetricSyncError::ProviderApi(format!(
                    "TikTok token refresh failed: HTTP {} — connection marked as expired, re-auth required",
                    refresh_response.status()
                )));
            }

            // TikTok's /v2/oauth/token/ endpoint returns fields at the root
            // level (not wrapped in a "data" object), exactly like the
            // authorization-code exchange in the OAuth callback. The callback
            // handler documents this explicitly — see connections_tiktok.rs.
            let refresh_data: serde_json::Value = refresh_response.json().await?;

            // Refresh MUST return a new access_token. If it doesn't, the
            // refresh failed — do NOT fall back to the old expired token.
            let new_access_token = refresh_data
                .get("access_token")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    GrowthMetricSyncError::ProviderApi(
                        "TikTok refresh response missing access_token — cannot use old expired token".to_string(),
                    )
                })?;
            // Some providers return a new refresh_token, some don't.
            // If a new one is provided, use it; otherwise keep the old one.
            let new_refresh_token = refresh_data
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .unwrap_or(&refresh_token);
            let expires_in = refresh_data
                .get("expires_in")
                .and_then(|v| v.as_i64())
                .unwrap_or(86400);
            let new_expires_at = now + time::Duration::seconds(expires_in.saturating_sub(60));

            // Encrypt and persist the refreshed tokens.
            let enc_access = URL_SAFE_NO_PAD.encode(
                encrypt_value(
                    new_access_token.as_bytes(),
                    &self.response_encryption_key,
                    &aad,
                )
                .map_err(|_| {
                    GrowthMetricSyncError::ProviderApi(
                        "failed to encrypt refreshed access token".to_string(),
                    )
                })?,
            );
            let enc_refresh = URL_SAFE_NO_PAD.encode(
                encrypt_value(
                    new_refresh_token.as_bytes(),
                    &self.response_encryption_key,
                    &aad,
                )
                .map_err(|_| {
                    GrowthMetricSyncError::ProviderApi(
                        "failed to encrypt refreshed refresh token".to_string(),
                    )
                })?,
            );
            sqlx::query(
                r#"UPDATE fanbase_connections
                   SET encrypted_access_token = $1,
                       encrypted_refresh_token = $2,
                       token_expires_at = $3,
                       status = 'connected',
                       updated_at = now()
                   WHERE id = $4"#,
            )
            .bind(&enc_access)
            .bind(&enc_refresh)
            .bind(new_expires_at)
            .bind(conn.id)
            .execute(&self.pool)
            .await?;

            access_token = new_access_token.to_string();
            // refresh_token is not used after this point — the new value
            // was already persisted in the UPDATE above. Keeping the old
            // variable would be misleading, so we drop it.

            tracing::info!(
                connection_id = %conn.id,
                open_id = open_id,
                "TikTok access token refreshed"
            );
        }

        // Fetch user info with follower_count.
        let user_info_response = self
            .http_client
            .get("https://open.tiktokapis.com/v2/user/info/")
            .query(&[(
                "fields",
                "open_id,display_name,follower_count,following_count,likes_count,video_count",
            )])
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await?;

        if !user_info_response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "TikTok user info failed: HTTP {}",
                user_info_response.status()
            )));
        }

        let user_info: serde_json::Value = user_info_response.json().await?;

        // Check for API-level error.
        if let Some(error) = user_info.get("error")
            && error.get("code").and_then(|v| v.as_str()) != Some("ok")
        {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "TikTok API error: {}",
                error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            )));
        }

        let user = user_info
            .get("data")
            .and_then(|d| d.get("user"))
            .ok_or_else(|| {
                GrowthMetricSyncError::ProviderApi("TikTok response missing user data".to_string())
            })?;

        let follower_count = user
            .get("follower_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let display_name = user
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or("TikTok creator");

        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "tiktok",
            "followers",
            &format!("TikTok followers — {display_name}"),
            follower_count,
            OffsetDateTime::now_utc(),
        )
        .await?;

        tracing::info!(
            connection_id = %conn.id,
            open_id = open_id,
            artist = %display_name,
            followers = follower_count,
            "tiktok follower count recorded"
        );
        Ok(())
    }

    /// Reddit: subscriber count for one subreddit, read through the agent
    /// service. Recorded under platform='social' because the MetricPlatform
    /// enum has no 'reddit' variant — Reddit feeds the social coverage bucket.
    ///
    /// This used to fetch `https://www.reddit.com/r/{sub}/about.json` directly,
    /// through a static proxy, then a rotating free-proxy pool, then a direct
    /// connection — three tiers of fallback built on the premise that Reddit
    /// blocks datacenter IPs and a different IP would get through.
    ///
    /// The premise is false. Measured against Reddit today, from a datacenter
    /// host and a residential connection, with a curl and a browser
    /// User-Agent:
    ///
    /// ```text
    /// GET https://www.reddit.com/r/Metal/about.json   403   (both)
    /// GET https://old.reddit.com/r/Metal/about.json   302 to /login
    /// GET https://www.reddit.com/r/Metal/             200, but the body is a
    ///                                                 JavaScript proof-of-work
    ///                                                 challenge, not the
    ///                                                 subreddit
    /// ```
    ///
    /// Reddit requires authentication for the JSON API now, from everywhere.
    /// No proxy reaches around an auth requirement, so every tier of that
    /// fallback was guaranteed to fail and the failure was silent: production
    /// carries 29 connected Reddit connections and not one of them has ever
    /// recorded a data point.
    ///
    /// The agent service is the only route that holds a credential, so this
    /// asks it, exactly as the community-intelligence adapter does. The moment
    /// Reddit access is restored — through the browser session or a script
    /// app — all 29 connections start syncing without another change here.
    async fn sync_reddit(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let subreddit = &conn.provider_account_id;
        let Some(auth_key) = self.agent_service_auth_key.as_deref() else {
            return Err(GrowthMetricSyncError::ProviderApi(
                "reddit needs the agent service to read a subreddit and                  CROWDRELAY_AGENT_SERVICE_AUTH_KEY is not set"
                    .to_owned(),
            ));
        };
        let url = format!("{}/reddit/observe", self.agent_service_url);
        let token = crate::discovery::derive_agent_token(auth_key, conn.workspace_id);
        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Workspace-Id", conn.workspace_id.to_string())
            .json(&serde_json::json!({ "subreddit": subreddit, "limit": 1 }))
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let detail = response
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|body| {
                    body.get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "agent service could not read r/{subreddit}: {detail}"
            )));
        }
        let body: RedditObserveResponse = response.json().await?;
        let Some(subscriber_count) = body.subscribers else {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "agent service returned no subscriber count for r/{subreddit}"
            )));
        };

        let display_name = body.title.as_deref().unwrap_or(subreddit);
        record_metric_point(
            &self.pool,
            conn.workspace_id,
            conn.id,
            "social",
            "subscribers",
            &format!("Reddit subscribers — r/{display_name}"),
            subscriber_count,
            OffsetDateTime::now_utc(),
        )
        .await?;

        tracing::info!(
            connection_id = %conn.id,
            subreddit = %subreddit,
            subscribers = subscriber_count,
            "reddit subreddit subscriber count recorded"
        );
        Ok(())
    }
}

#[derive(Debug, FromRow)]
struct DueConnectionRow {
    id: Uuid,
    workspace_id: Uuid,
    platform: String,
    provider_account_id: String,
    external_account_ref: String,
}

#[derive(Clone, Debug)]
struct DueConnection {
    id: Uuid,
    workspace_id: Uuid,
    platform: String,
    provider_account_id: String,
    external_account_ref: String,
}

// --- YouTube response types ---

#[derive(Debug, Deserialize)]
struct YoutubeChannelsResponse {
    items: Vec<YoutubeChannelItem>,
}

#[derive(Debug, Deserialize)]
struct YoutubeChannelItem {
    statistics: YoutubeChannelStatistics,
}

#[derive(Debug, Deserialize)]
struct YoutubeChannelStatistics {
    /// YouTube returns subscriberCount as a string in some responses and as
    /// a number in others. Accept both.
    #[serde(rename = "subscriberCount")]
    subscriber_count: Option<serde_json::Value>,
}

// --- Spotify response types (internal partner API) ---

/// Top-level response from Spotify's pathfinder GraphQL API.
#[derive(Debug, Deserialize)]
struct SpotifyPartnerArtistResponse {
    data: SpotifyPartnerData,
}

#[derive(Debug, Deserialize)]
struct SpotifyPartnerData {
    artist: SpotifyPartnerArtist,
}

#[derive(Debug, Deserialize)]
struct SpotifyPartnerArtist {
    #[serde(default)]
    stats: Option<SpotifyPartnerStats>,
    profile: SpotifyPartnerProfile,
}

#[derive(Debug, Deserialize)]
struct SpotifyPartnerStats {
    followers: i64,
    #[serde(default, rename = "monthlyListeners")]
    monthly_listeners: i64,
    #[allow(dead_code)]
    #[serde(default, rename = "worldRank")]
    world_rank: i64,
}

#[derive(Debug, Deserialize)]
struct SpotifyPartnerProfile {
    name: String,
}

// --- Facebook response types ---

#[derive(Debug, Deserialize)]
struct FacebookPageResponse {
    name: Option<String>,
    /// Number of users who like the page. On New Pages Experience pages
    /// this may equal followers_count.
    #[serde(default)]
    fan_count: i64,
    /// Number of page followers. May be absent on some page types.
    #[serde(default)]
    followers_count: Option<i64>,
}

// --- Instagram response types ---

#[derive(Debug, Deserialize)]
struct InstagramUserResponse {
    username: Option<String>,
    #[serde(default)]
    followers_count: i64,
}

// --- TikTok helpers ---

/// Associated data for OAuth token encryption. Must match the AAD used
/// by `PostgresFanbaseRepository::oauth_aad` in the infra crate.
fn tiktok_oauth_aad(workspace_id: Uuid, open_id: &str) -> Vec<u8> {
    format!("crowdrelay.fanbase.oauth.tiktok.v1\0{workspace_id}\0{open_id}").into_bytes()
}

// --- SoundCloud response types ---

/// Subset of SoundCloud's user hydration data. The full object has many
/// more fields, but we only care about followers and the display name.
#[derive(Debug, Deserialize)]
struct SoundCloudUserData {
    username: String,
    #[serde(default)]
    followers_count: i64,
}

// --- X (Twitter) response types ---

/// Subset of the schema.org `Person` entity that X server-renders as
/// JSON-LD in the profile page HTML. We only need the follower count
/// and display name for the growth metric series.
#[derive(Debug, Deserialize)]
struct XProfileData {
    name: String,
    #[serde(default, rename = "interactionStatistic")]
    interaction_statistic: Vec<XInteractionStatistic>,
}

#[derive(Debug, Deserialize)]
struct XInteractionStatistic {
    #[serde(rename = "interactionType")]
    interaction_type: String,
    #[serde(rename = "userInteractionCount")]
    user_interaction_count: i64,
}

/// Extracts the profile data from X's server-rendered JSON-LD.
/// X embeds a `<script type="application/ld+json">` block containing
/// a `ProfilePage` whose `mainEntity` is a `Person` with
/// `interactionStatistic` entries. The `Follow` interaction type
/// carries the follower count.
fn extract_x_profile_data(html: &str) -> Option<XProfileData> {
    // Find all JSON-LD script blocks and look for the one containing
    // a Person with interactionStatistic.
    let marker = r#"type="application/ld+json">"#;
    let mut remaining = html;
    while let Some(rel_start) = remaining.find(marker) {
        let after_marker = remaining.get(rel_start + marker.len()..)?;
        let end = after_marker.find("</script>")?;
        let json_str = after_marker.get(..end)?;
        remaining = after_marker.get(end..).unwrap_or("");

        // The JSON-LD can be a single object or an array of objects.
        // X typically renders a ProfilePage with a mainEntity property.
        // Try parsing as a flexible JSON value first, then navigate.
        let parsed: serde_json::Value = serde_json::from_str(json_str.trim()).ok()?;

        // Navigate: top-level could be the Person directly, or a
        // ProfilePage wrapping it, or an array of either.
        if let Some(person) = find_person_in_json_ld(&parsed) {
            let profile: XProfileData = serde_json::from_value(person).ok()?;
            return Some(profile);
        }
    }
    None
}

/// Recursively searches a JSON-LD value for a `Person` typed object
/// (by `@type` or schema.org type) and returns it.
fn find_person_in_json_ld(value: &serde_json::Value) -> Option<serde_json::Value> {
    // Check if this object is a Person.
    if let Some(obj) = value.as_object() {
        let type_field = obj.get("@type").or_else(|| obj.get("type"))?;
        let is_person = type_field
            .as_str()
            .map(|t| t == "Person" || t.contains("Person"))
            .unwrap_or(false)
            || type_field.as_array().is_some_and(|arr| {
                arr.iter().any(|v| {
                    v.as_str()
                        .is_some_and(|t| t == "Person" || t.contains("Person"))
                })
            });
        if is_person {
            return Some(value.clone());
        }
        // Check mainEntity (ProfilePage → Person).
        if let Some(main_entity) = obj.get("mainEntity")
            && let Some(person) = find_person_in_json_ld(main_entity)
        {
            return Some(person);
        }
        // Check about (some pages use this).
        if let Some(about) = obj.get("about")
            && let Some(person) = find_person_in_json_ld(about)
        {
            return Some(person);
        }
    }
    // Search array elements.
    if let Some(arr) = value.as_array() {
        for element in arr {
            if let Some(person) = find_person_in_json_ld(element) {
                return Some(person);
            }
        }
    }
    None
}

/// Extracts the follower count from an `XProfileData` by finding the
/// `Follow` interaction statistic entry.
impl XProfileData {
    fn follower_count(&self) -> i64 {
        self.interaction_statistic
            .iter()
            .find(|stat| {
                stat.interaction_type.ends_with("FollowAction")
                    || stat.interaction_type.ends_with("Follow")
            })
            .map(|stat| stat.user_interaction_count)
            .unwrap_or(0)
    }
}

/// Extracts the user data JSON from SoundCloud's `window.__sc_hydration`
/// array. The hydration data is an array of objects, each with a
/// `hydratable` field and a `data` field. The user object has
/// `hydratable: "user"`.
fn extract_soundcloud_user_data(html: &str) -> Option<SoundCloudUserData> {
    let marker = "window.__sc_hydration = ";
    let start = html.find(marker)? + marker.len();
    let rest = html.get(start..)?;
    let end = rest.find(";</script>")?;
    let json_str = rest.get(..end)?;
    let hydration: Vec<serde_json::Value> = serde_json::from_str(json_str).ok()?;
    for entry in &hydration {
        if entry.get("hydratable").and_then(|v| v.as_str()) == Some("user") {
            let data = entry.get("data")?;
            let user: SoundCloudUserData = serde_json::from_value(data.clone()).ok()?;
            return Some(user);
        }
    }
    None
}

// --- Spotify embed token extraction ---

/// Extracts the public web player access token from a Spotify embed page's
/// HTML. The token appears as "accessToken":"<token>" in the HTML. This is
/// the same token the Spotify web player uses — it's freely given to anyone
/// loading the embed page, no authentication required.
fn extract_spotify_embed_token(html: &str) -> Option<String> {
    let marker = "\"accessToken\":\"";
    let start = html.find(marker)? + marker.len();
    let rest = html.get(start..)?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_string())
}

/// Minimal URL-encoder for GraphQL query parameters. Spotify's pathfinder
/// API expects the variables and extensions JSON to be URL-encoded in the
/// query string.
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

// --- Reddit response types ---

/// The subset of `POST /reddit/observe` this worker reads. The endpoint
/// reports far more about a community; a subscriber count is all a metric
/// point needs, and naming only that keeps the two from drifting into a
/// shared shape neither owns.
#[derive(Debug, Deserialize)]
struct RedditObserveResponse {
    subscribers: Option<i64>,
    title: Option<String>,
}

/// Accepts a count only where it is a whole, non-negative number. YouTube
/// returns subscriber counts as strings in some responses and numbers in
/// others; this normalizes both.
fn normalize_count(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => n.as_i64().filter(|c| *c >= 0),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok().filter(|c| *c >= 0),
        _ => None,
    }
}

/// Records a metric point, declaring the series on first sight.
/// Same pattern as the Bandsintown tracker: INSERT ... ON CONFLICT DO NOTHING
/// for the point, INSERT ... ON CONFLICT DO UPDATE for the series.
#[allow(clippy::too_many_arguments)]
async fn record_metric_point(
    pool: &PgPool,
    workspace_id: Uuid,
    connection_id: Uuid,
    platform: &str,
    metric_key: &str,
    display_name: &str,
    value: i64,
    observed_at: OffsetDateTime,
) -> Result<(), GrowthMetricSyncError> {
    // The series is scoped to the fanbase connection, not the workspace:
    // a workspace may have multiple Meta pages or YouTube channels, and a
    // workspace-level series would interleave their numbers.
    sqlx::query(
        r#"
        WITH series AS (
            INSERT INTO viryaos_growth_metric_series (
                workspace_id, platform, metric_key, subject_kind, subject_id,
                display_name, direction, value_tier, expected_interval_hours, active
            )
            VALUES (
                $1, $2, $3, 'fanbase_connection', $4,
                left($5, 120),
                'higher_is_better', 'intermediate', 24, true
            )
            ON CONFLICT (workspace_id, platform, metric_key, subject_kind, subject_id)
            DO UPDATE SET
                display_name = EXCLUDED.display_name,
                active = true
            RETURNING id
        )
        INSERT INTO viryaos_growth_metric_points (
            workspace_id, series_id, captured_at, value, source
        )
        SELECT $1, series.id, date_trunc('hour', $6::timestamptz), $7, 'growth_metric_sync'
        FROM series
        ON CONFLICT (workspace_id, series_id, captured_at) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(platform)
    .bind(metric_key)
    .bind(connection_id)
    .bind(display_name)
    .bind(observed_at)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::fanbase::Platform as ConnectionPlatform;

    #[test]
    fn synced_platforms_match_the_domain() {
        // SYNCED_PLATFORMS is a literal list so the Python vocabulary contract
        // can read it, but the enum decides what belongs in it. Adding a
        // connection platform and answering `polled_by_growth_metric_sync`
        // fails here until the literal list is updated to match — which is the
        // point: the list is checked, not trusted.
        let expected: Vec<&str> = ConnectionPlatform::ALL
            .into_iter()
            .filter(|platform| platform.polled_by_growth_metric_sync())
            .map(ConnectionPlatform::as_str)
            .collect();
        let mut actual = SYNCED_PLATFORMS.to_vec();
        let mut expected_sorted = expected.clone();
        actual.sort_unstable();
        expected_sorted.sort_unstable();
        assert_eq!(
            actual, expected_sorted,
            "SYNCED_PLATFORMS drifted from Platform::polled_by_growth_metric_sync"
        );
    }

    #[test]
    fn youtube_response_parses_subscriber_count() {
        // YouTube returns subscriberCount as a string.
        let json = br#"{"items":[{"id":"UC123","statistics":{"subscriberCount":"842","viewCount":"100000"}}]}"#;
        let response: YoutubeChannelsResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(
            response
                .items
                .first()
                .and_then(|i| i.statistics.subscriber_count.as_ref())
                .and_then(normalize_count),
            Some(842)
        );
    }

    #[test]
    fn youtube_response_with_no_items_is_handled() {
        let json = br#"{"items":[]}"#;
        let response: YoutubeChannelsResponse = serde_json::from_slice(json).unwrap();
        assert!(response.items.is_empty());
    }

    #[test]
    fn tiktok_refresh_response_parses_from_root() {
        // TikTok's /v2/oauth/token/ endpoint returns access_token,
        // refresh_token, and expires_in at the root level — NOT wrapped
        // in a "data" object. This is the same endpoint the OAuth callback
        // uses (grant_type=authorization_code); the refresh uses
        // grant_type=refresh_token but hits the same URL and gets the same
        // response shape. The sync worker must read from the root.
        let json = br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":86400,"open_id":"abc","scope":"user.info.basic"}"#;
        let refresh_data: serde_json::Value = serde_json::from_slice(json).unwrap();

        // The old code read refresh_data.get("data") and always failed.
        // Reading from the root must succeed.
        assert!(refresh_data.get("data").is_none());
        let access_token = refresh_data
            .get("access_token")
            .and_then(|v| v.as_str())
            .expect("access_token at root");
        let refresh_token = refresh_data
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .expect("refresh_token at root");
        let expires_in = refresh_data
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .expect("expires_in at root");
        assert_eq!(access_token, "new-access");
        assert_eq!(refresh_token, "new-refresh");
        assert_eq!(expires_in, 86400);
    }

    #[test]
    fn spotify_partner_response_parses_stats() {
        let json = br#"{"data":{"artist":{"id":"6bbW0jOKAWJWm3h6CTWaAS","uri":"spotify:artist:6bbW0jOKAWJWm3h6CTWaAS","profile":{"name":"Virya","verified":true},"stats":{"followers":183,"monthlyListeners":45,"worldRank":0}}}}"#;
        let response: SpotifyPartnerArtistResponse = serde_json::from_slice(json).unwrap();
        let stats = response.data.artist.stats.unwrap();
        assert_eq!(response.data.artist.profile.name, "Virya");
        assert_eq!(stats.followers, 183);
        assert_eq!(stats.monthly_listeners, 45);
    }

    #[test]
    fn spotify_partner_response_parses_without_stats() {
        let json =
            br#"{"data":{"artist":{"id":"6bbW0jOKAWJWm3h6CTWaAS","profile":{"name":"Virya"}}}}"#;
        let response: SpotifyPartnerArtistResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(response.data.artist.profile.name, "Virya");
        assert!(response.data.artist.stats.is_none());
    }

    #[test]
    fn extract_spotify_embed_token_finds_token() {
        let html = r#"<html><script id="__NEXT_DATA__">{"props":{"accessToken":"BQxyz123abc"}}</script></html>"#;
        let token = extract_spotify_embed_token(html);
        assert_eq!(token.as_deref(), Some("BQxyz123abc"));
    }

    #[test]
    fn extract_spotify_embed_token_returns_none_when_missing() {
        let html = r#"<html><body>no token here</body></html>"#;
        let token = extract_spotify_embed_token(html);
        assert!(token.is_none());
    }

    #[test]
    fn urlencode_encodes_special_chars() {
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(urlencode(r#"{"key":"val"}"#), "%7B%22key%22%3A%22val%22%7D");
        assert_eq!(urlencode("abc123-_.~"), "abc123-_.~");
    }

    #[test]
    fn facebook_page_response_parses_followers() {
        let json =
            br#"{"name":"Virya","fan_count":1256,"followers_count":1256,"id":"101848539107631"}"#;
        let response: FacebookPageResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(response.name.as_deref(), Some("Virya"));
        assert_eq!(response.fan_count, 1256);
        assert_eq!(response.followers_count, Some(1256));
    }

    #[test]
    fn facebook_page_response_without_followers_count() {
        // Some page types may omit followers_count — fall back to fan_count.
        let json = br#"{"name":"Virya","fan_count":999,"id":"101848539107631"}"#;
        let response: FacebookPageResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(response.fan_count, 999);
        assert!(response.followers_count.is_none());
    }

    #[test]
    fn instagram_user_response_parses_followers() {
        let json =
            br#"{"username":"virya.official","followers_count":522,"id":"17841455886865962"}"#;
        let response: InstagramUserResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(response.username.as_deref(), Some("virya.official"));
        assert_eq!(response.followers_count, 522);
    }

    #[test]
    fn reddit_observe_response_parses_subscribers() {
        let json = br#"{"subreddit":"Metal","title":"Metal","subscribers":1983452,"posts":[]}"#;
        let response: RedditObserveResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(response.subscribers, Some(1983452));
        assert_eq!(response.title.as_deref(), Some("Metal"));
    }

    #[test]
    fn reddit_observe_response_tolerates_a_missing_count() {
        // The endpoint reports `subscribers: null` for a community it could
        // read but whose size Reddit did not return. Recording a zero there
        // would put a fabricated low point in the series and read as a
        // collapse in audience.
        let json = br#"{"subreddit":"Metal","title":null,"subscribers":null,"posts":[]}"#;
        let response: RedditObserveResponse = serde_json::from_slice(json).unwrap();
        assert!(response.subscribers.is_none());
        assert!(response.title.is_none());
    }

    #[test]
    fn soundcloud_user_data_parses_followers() {
        let json =
            br#"{"username":"Virya","followers_count":11,"followings_count":0,"track_count":14}"#;
        let user: SoundCloudUserData = serde_json::from_slice(json).unwrap();
        assert_eq!(user.username, "Virya");
        assert_eq!(user.followers_count, 11);
    }

    #[test]
    fn soundcloud_user_data_parses_without_followers() {
        let json = br#"{"username":"TestArtist"}"#;
        let user: SoundCloudUserData = serde_json::from_slice(json).unwrap();
        assert_eq!(user.username, "TestArtist");
        assert_eq!(user.followers_count, 0);
    }

    #[test]
    fn extract_soundcloud_user_data_finds_user() {
        let html = r#"<html><head></head><body><script>window.__sc_hydration = [{"hydratable":"sound","data":{"id":1}},{"hydratable":"user","data":{"username":"Virya","followers_count":11,"id":1176127912}}];</script></body></html>"#;
        let user = extract_soundcloud_user_data(html);
        assert!(user.is_some());
        let user = user.unwrap();
        assert_eq!(user.username, "Virya");
        assert_eq!(user.followers_count, 11);
    }

    #[test]
    fn extract_soundcloud_user_data_returns_none_when_missing() {
        let html = r#"<html><body>no hydration data here</body></html>"#;
        let user = extract_soundcloud_user_data(html);
        assert!(user.is_none());
    }

    #[test]
    fn extract_soundcloud_user_data_returns_none_when_no_user_entry() {
        let html = r#"<html><script>window.__sc_hydration = [{"hydratable":"sound","data":{"id":1}}];</script></html>"#;
        let user = extract_soundcloud_user_data(html);
        assert!(user.is_none());
    }

    #[test]
    fn lastfm_response_parses_listener_and_playcount() {
        let json = serde_json::json!({
            "artist": {
                "name": "Iron Maiden",
                "stats": {
                    "listeners": "1548327",
                    "playcount": "58392104"
                }
            }
        });
        let listeners = json
            .get("artist")
            .and_then(|a| a.get("stats"))
            .and_then(|s| s.get("listeners"))
            .and_then(|l| l.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let playcount = json
            .get("artist")
            .and_then(|a| a.get("stats"))
            .and_then(|s| s.get("playcount"))
            .and_then(|p| p.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        assert_eq!(listeners, 1_548_327);
        assert_eq!(playcount, 58_392_104);
    }

    #[test]
    fn lastfm_response_without_stats_returns_zero() {
        let json = serde_json::json!({"artist": {"name": "Unknown"}});
        let listeners = json
            .get("artist")
            .and_then(|a| a.get("stats"))
            .and_then(|s| s.get("listeners"))
            .and_then(|l| l.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        assert_eq!(listeners, 0);
    }

    #[test]
    fn deezer_response_parses_fan_count() {
        let json = serde_json::json!({"id": 13, "name": "Eminem", "nb_fan": 1234567});
        let fan_count = json
            .get("nb_fan")
            .and_then(serde_json::Value::as_i64)
            .expect("fan count");
        assert_eq!(fan_count, 1_234_567);
    }

    #[test]
    fn deezer_error_response_is_detected() {
        let json = serde_json::json!({"error": {"code": 800, "message": "no such artist"}});
        assert!(json.get("error").is_some());
    }

    #[test]
    fn discogs_response_parses_community_stats() {
        let json = serde_json::json!({
            "id": 18839,
            "name": "Iron Maiden",
            "stats": {"community": {"in_collection": 45678, "in_wantlist": 12345}}
        });
        let stats = json.get("stats").and_then(|s| s.get("community"));
        let in_collection = stats
            .and_then(|s| s.get("in_collection"))
            .and_then(serde_json::Value::as_i64)
            .expect("in_collection");
        let in_wantlist = stats
            .and_then(|s| s.get("in_wantlist"))
            .and_then(serde_json::Value::as_i64)
            .expect("in_wantlist");
        assert_eq!(in_collection, 45_678);
        assert_eq!(in_wantlist, 12_345);
    }

    #[test]
    fn bluesky_response_parses_followers_count() {
        let json = serde_json::json!({"did": "did:plc:abc", "handle": "virya.bsky.social", "followersCount": 98765});
        let followers = json
            .get("followersCount")
            .and_then(serde_json::Value::as_i64)
            .expect("followers");
        assert_eq!(followers, 98_765);
    }

    #[test]
    fn bandcamp_html_supporter_count_is_parsed() {
        let html = r#"<div class="community-recent-supporters">
    <h2 class="heading">Recent Supporters</h2>
    <ol class="supporters">
        <li><a href="https://bandcamp.com/gierula" class="supporter">Fan 1</a></li>
        <li><a href="https://bandcamp.com/tasjonde" class="supporter">Fan 2</a></li>
        <li><a href="https://bandcamp.com/andbia" class="supporter">Fan 3</a></li>
    </ol>
</div>"#;
        let count: i64 = html
            .matches(r#"class="supporter""#)
            .count()
            .try_into()
            .unwrap_or(0);
        assert_eq!(count, 3);
    }

    #[test]
    fn bandcamp_html_without_supporters_section_is_detected() {
        let html = r#"<html><body><h1>Page not found</h1></body></html>"#;
        assert!(
            !html.contains(r#"class="supporters""#)
                && !html.contains("community-recent-supporters")
        );
    }

    #[test]
    fn x_profile_data_parses_followers() {
        let json = r#"{"@type":"Person","name":"Virya","interactionStatistic":[{"@type":"InteractionCounter","interactionType":"https://schema.org/FollowAction","userInteractionCount":12345},{"@type":"InteractionCounter","interactionType":"https://schema.org/LikeAction","userInteractionCount":999}]}"#;
        let profile: XProfileData = serde_json::from_str(json).unwrap();
        assert_eq!(profile.name, "Virya");
        assert_eq!(profile.follower_count(), 12345);
    }

    #[test]
    fn x_profile_data_parses_without_followers() {
        let json = r#"{"@type":"Person","name":"TestArtist"}"#;
        let profile: XProfileData = serde_json::from_str(json).unwrap();
        assert_eq!(profile.name, "TestArtist");
        assert_eq!(profile.follower_count(), 0);
    }

    #[test]
    fn extract_x_profile_data_finds_person_in_profile_page() {
        let html = r#"<html><head><script type="application/ld+json">{"@type":"ProfilePage","mainEntity":{"@type":"Person","name":"Virya","interactionStatistic":[{"@type":"InteractionCounter","interactionType":"https://schema.org/FollowAction","userInteractionCount":5000}]}}</script></head><body></body></html>"#;
        let profile = extract_x_profile_data(html);
        assert!(profile.is_some());
        let profile = profile.unwrap();
        assert_eq!(profile.name, "Virya");
        assert_eq!(profile.follower_count(), 5000);
    }

    #[test]
    fn extract_x_profile_data_finds_standalone_person() {
        let html = r#"<html><head><script type="application/ld+json">{"@type":"Person","name":"IndieArtist","interactionStatistic":[{"@type":"InteractionCounter","interactionType":"https://schema.org/FollowAction","userInteractionCount":42}]}</script></head></html>"#;
        let profile = extract_x_profile_data(html);
        assert!(profile.is_some());
        let profile = profile.unwrap();
        assert_eq!(profile.name, "IndieArtist");
        assert_eq!(profile.follower_count(), 42);
    }

    #[test]
    fn extract_x_profile_data_returns_none_when_missing() {
        let html = r#"<html><body>no json-ld here</body></html>"#;
        let profile = extract_x_profile_data(html);
        assert!(profile.is_none());
    }

    #[test]
    fn extract_x_profile_data_returns_none_when_no_person() {
        let html = r#"<html><head><script type="application/ld+json">{"@type":"WebPage","name":"Not a profile"}</script></head></html>"#;
        let profile = extract_x_profile_data(html);
        assert!(profile.is_none());
    }
}
