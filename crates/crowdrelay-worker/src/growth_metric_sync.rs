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

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_infra::sensitive_response::{SensitiveResponseKey, decrypt_value, encrypt_value};
use serde::Deserialize;
use sqlx::{FromRow, PgPool, postgres::PgListener};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::{Mutex, watch},
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

/// Free proxy list sources. Fetched at startup and when the pool is
/// exhausted. Each returns a newline-separated list of `host:port` entries.
const PROXY_SOURCES: &[&str] = &[
    "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/http.txt",
    "https://api.proxyscrape.com/v2/?request=getproxies&protocol=http&timeout=5000&country=all",
    "https://www.proxy-list.download/api/v1/get?type=http",
];

/// How many proxies to test concurrently when refreshing the pool.
const PROXY_TEST_CONCURRENCY: usize = 20;
/// Timeout for a single proxy test (connect + fetch Reddit about.json).
const PROXY_TEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Maximum number of working proxies to keep in the pool.
const PROXY_POOL_MAX: usize = 10;
/// How long to keep a proxy in the pool before forcing a refresh.
const PROXY_POOL_TTL: Duration = Duration::from_secs(30 * 60);

/// Platforms the growth metric sync worker handles. The fanbase_connection
/// platform is the connectable surface; the metric series platform is the
/// coverage bucket. Reddit connections record under 'social' because the
/// MetricPlatform enum has no 'reddit' variant — Reddit feeds the social
/// coverage bucket.
const SYNCED_PLATFORMS: &[&str] = &[
    "youtube",
    "spotify",
    "tiktok",
    "facebook",
    "instagram",
    "soundcloud",
    "reddit",
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
    /// Static proxy override. If set, the worker uses this single proxy
    /// for all Reddit requests instead of the free proxy pool.
    reddit_static_proxy: Option<String>,
    /// Rotating free-proxy pool for Reddit. Lazily populated on first
    /// Reddit sync and refreshed when exhausted or stale.
    reddit_proxy_pool: Arc<Mutex<RedditProxyPool>>,
    /// TikTok Display API credentials for OAuth token refresh.
    tiktok_client_key: Option<String>,
    tiktok_client_secret: Option<String>,
    /// Encryption key for decrypting OAuth tokens stored in
    /// `encrypted_access_token` / `encrypted_refresh_token`.
    response_encryption_key: SensitiveResponseKey,
    operation_timeout: Duration,
}

impl GrowthMetricSyncWorker {
    /// Creates a new worker. Returns `Ok(None)` if no platform is configured
    /// (no YouTube API key and no Facebook token) — the caller should
    /// not spawn the worker in that case. Reddit needs no credentials, so
    /// the worker is enabled as long as any other platform is configured.
    /// Spotify needs no credentials either — it uses Spotify's public embed
    /// page to obtain a web player token.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        youtube_api_key: Option<String>,
        facebook_page_access_token: Option<String>,
        reddit_proxy_url: Option<String>,
        tiktok_client_key: Option<String>,
        tiktok_client_secret: Option<String>,
        response_encryption_key: SensitiveResponseKey,
        operation_timeout: Duration,
    ) -> Result<Option<Self>, GrowthMetricSyncError> {
        if youtube_api_key.is_none()
            && facebook_page_access_token.is_none()
            && tiktok_client_key.is_none()
        {
            tracing::info!("growth metric sync disabled: no platform credentials configured");
            return Ok(None);
        }
        // The main HTTP client is used for YouTube and Spotify — no proxy.
        // Reddit gets its own per-request clients with proxies.
        let http_client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT.min(Duration::from_secs(10)))
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(GrowthMetricSyncError::ClientBuild)?;
        if let Some(ref url) = reddit_proxy_url {
            tracing::info!("growth metric sync: using static Reddit proxy: {url}");
        } else {
            tracing::info!("growth metric sync: using free proxy pool for Reddit");
        }
        Ok(Some(Self {
            pool,
            http_client,
            youtube_api_key,
            facebook_page_access_token,
            reddit_static_proxy: reddit_proxy_url,
            reddit_proxy_pool: Arc::new(Mutex::new(RedditProxyPool::new())),
            tiktok_client_key,
            tiktok_client_secret,
            response_encryption_key,
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
        self.sync_cycle().await;

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
                    self.sync_cycle().await;
                }
                // Scheduled wake: a connection's next sync time arrived.
                _ = sleep(sleep_duration) => {
                    self.sync_cycle().await;
                }
            }
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
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(
                            connection_id = %conn.id,
                            platform = %conn.platform,
                            error = %error,
                            "growth metric sync failed for connection"
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            connection_id = %conn.id,
                            platform = %conn.platform,
                            "growth metric sync timed out for connection (20s)"
                        );
                    }
                }
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
                fc.id, fc.workspace_id, fc.platform, fc.provider_account_id
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
            ORDER BY fc.created_at
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
            _ => Ok(()),
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
        let response = self.http_client.get(&url).send().await?;
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
        let response = self.http_client.get(&url).send().await?;
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
        let response = self.http_client.get(&url).send().await?;
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

            let refresh_data: serde_json::Value = refresh_response.json().await?;
            let data = refresh_data.get("data").ok_or_else(|| {
                GrowthMetricSyncError::ProviderApi(
                    "TikTok refresh response missing data".to_string(),
                )
            })?;

            // Refresh MUST return a new access_token. If it doesn't, the
            // refresh failed — do NOT fall back to the old expired token.
            let new_access_token = data
                .get("access_token")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    GrowthMetricSyncError::ProviderApi(
                        "TikTok refresh response missing access_token — cannot use old expired token".to_string(),
                    )
                })?;
            // Some providers return a new refresh_token, some don't.
            // If a new one is provided, use it; otherwise keep the old one.
            let new_refresh_token = data
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .unwrap_or(&refresh_token);
            let expires_in = data
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

    /// Reddit: fetch subreddit subscriber count via public JSON (no auth).
    /// Recorded under platform='social' because the MetricPlatform enum has
    /// no 'reddit' variant — Reddit feeds the social coverage bucket.
    ///
    /// Reddit blocks datacenter IPs. The worker tries:
    ///   1. A static proxy if `CROWDRELAY_REDDIT_PROXY_URL` is set.
    ///   2. The free rotating proxy pool (fetched from public proxy lists,
    ///      tested against Reddit, cached for 30 minutes).
    ///   3. Direct connection as a last resort (may work from non-datacenter
    ///      IPs; logs a warning if it fails).
    async fn sync_reddit(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let subreddit = &conn.provider_account_id;
        let url = format!("https://www.reddit.com/r/{subreddit}/about.json");

        let response = self.fetch_reddit(&url).await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Reddit API returned HTTP {} for r/{subreddit}",
                response.status()
            )));
        }
        let body: RedditAboutResponse = response.json().await?;
        let subscriber_count = body.data.subscribers;

        let display_name = body.data.display_name.as_deref().unwrap_or(subreddit);
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

    /// Fetches a Reddit URL, trying the static proxy, then the proxy pool,
    /// then a direct connection.
    async fn fetch_reddit(&self, url: &str) -> Result<reqwest::Response, GrowthMetricSyncError> {
        // 1. Static proxy (if configured).
        if let Some(ref proxy_url) = self.reddit_static_proxy
            && let Ok(client) = self.build_proxied_client(proxy_url)
        {
            match client.get(url).send().await {
                Ok(resp) => return Ok(resp),
                Err(e) => tracing::warn!(
                    error = %e,
                    proxy = %proxy_url,
                    "reddit static proxy failed, falling back to pool"
                ),
            }
        }

        // 2. Free proxy pool.
        if let Some(proxy_url) = self.get_pool_proxy().await
            && let Ok(client) = self.build_proxied_client(&proxy_url)
        {
            match client.get(url).send().await {
                Ok(resp) => return Ok(resp),
                Err(e) => tracing::debug!(
                    error = %e,
                    proxy = %proxy_url,
                    "reddit pool proxy failed"
                ),
            }
            self.mark_proxy_failed(&proxy_url).await;
        }

        // 3. Direct connection (last resort — usually blocked from datacenter).
        tracing::debug!("reddit: trying direct connection (no proxy)");
        let resp = self.http_client.get(url).send().await?;
        if resp.status().is_success() {
            tracing::warn!(
                "reddit direct connection succeeded — datacenter IP not blocked \
                 (unexpected). Consider setting CROWDRELAY_REDDIT_PROXY_URL for reliability."
            );
        }
        Ok(resp)
    }

    /// Builds a reqwest client that routes through the given proxy.
    fn build_proxied_client(&self, proxy_url: &str) -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(proxy_url)?)
            .connect_timeout(PROXY_TEST_TIMEOUT)
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
    }

    /// Gets a working proxy from the pool, refreshing if stale or empty.
    async fn get_pool_proxy(&self) -> Option<String> {
        let mut pool = self.reddit_proxy_pool.lock().await;
        pool.get_proxy(&self.http_client).await
    }

    /// Marks a proxy as failed in the pool.
    async fn mark_proxy_failed(&self, proxy_url: &str) {
        let mut pool = self.reddit_proxy_pool.lock().await;
        pool.mark_failed(proxy_url);
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Reddit proxy pool
// ──────────────────────────────────────────────────────────────────────────

/// A rotating pool of free HTTP proxies for Reddit requests.
///
/// Reddit blocks datacenter IPs from accessing its public JSON API. The
/// pool fetches proxy lists from public sources, tests each proxy against
/// Reddit, and caches the working ones. When a proxy fails, it's evicted.
/// The pool refreshes when stale (30 min TTL) or empty.
struct RedditProxyPool {
    /// Working proxies, in order of discovery.
    working: Vec<String>,
    /// Round-robin index into `working`.
    next_idx: AtomicUsize,
    /// When the pool was last refreshed.
    last_refresh: Option<Instant>,
}

impl RedditProxyPool {
    fn new() -> Self {
        Self {
            working: Vec::new(),
            next_idx: AtomicUsize::new(0),
            last_refresh: None,
        }
    }

    /// Returns a working proxy, refreshing the pool if stale or empty.
    /// Returns `None` if no working proxy can be found.
    async fn get_proxy(&mut self, direct_client: &reqwest::Client) -> Option<String> {
        let needs_refresh = self.working.is_empty()
            || self
                .last_refresh
                .is_none_or(|t| t.elapsed() > PROXY_POOL_TTL);

        if needs_refresh {
            self.refresh(direct_client).await;
        }

        if self.working.is_empty() {
            return None;
        }

        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed) % self.working.len();
        self.working.get(idx).cloned()
    }

    /// Evicts a proxy that failed during a Reddit request.
    fn mark_failed(&mut self, proxy_url: &str) {
        let before = self.working.len();
        self.working.retain(|p| p != proxy_url);
        if self.working.len() < before {
            tracing::debug!(
                proxy = %proxy_url,
                remaining = self.working.len(),
                "evicted failed proxy from pool"
            );
        }
    }

    /// Fetches proxy lists from public sources, tests each proxy against
    /// Reddit's about.json, and keeps the ones that work (up to PROXY_POOL_MAX).
    async fn refresh(&mut self, direct_client: &reqwest::Client) {
        tracing::info!("reddit proxy pool: refreshing from public sources");

        // Fetch candidate proxies from all sources.
        let candidates = self.fetch_proxy_candidates(direct_client).await;
        if candidates.is_empty() {
            tracing::warn!("reddit proxy pool: no candidates from any source");
            self.last_refresh = Some(Instant::now());
            return;
        }

        tracing::info!(
            candidates = candidates.len(),
            "reddit proxy pool: testing candidates against Reddit"
        );

        // Test candidates concurrently.
        let working = test_proxies_concurrently(&candidates).await;

        tracing::info!(
            working = working.len(),
            candidates = candidates.len(),
            "reddit proxy pool: refresh complete"
        );

        self.working = working;
        self.next_idx.store(0, Ordering::Relaxed);
        self.last_refresh = Some(Instant::now());
    }

    /// Fetches proxy lists from all public sources concurrently and
    /// deduplicates. Each source is fetched independently — a failing
    /// source does not cancel the others. Worst-case latency is the
    /// timeout of a single source (~10s), not the sum of all sources.
    async fn fetch_proxy_candidates(&self, client: &reqwest::Client) -> Vec<String> {
        use tokio::task::JoinSet;

        let mut join_set: JoinSet<Option<Vec<String>>> = JoinSet::new();
        for source in PROXY_SOURCES {
            let client = client.clone();
            join_set.spawn(async move {
                match client
                    .get(*source)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        let text = resp.text().await.ok()?;
                        Some(
                            text.lines()
                                .map(str::trim)
                                .filter(|l| is_valid_proxy_line(l))
                                .map(|l| format!("http://{l}"))
                                .collect::<Vec<_>>(),
                        )
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, source = *source, "proxy source fetch failed");
                        None
                    }
                    _ => None,
                }
            });
        }

        let mut all: Vec<String> = Vec::new();
        while let Some(res) = join_set.join_next().await {
            if let Ok(Some(proxies)) = res {
                all.extend(proxies);
            }
        }
        all.sort();
        all.dedup();
        all
    }
}

/// Tests proxies concurrently against Reddit, returns the ones that work.
/// Stops spawning and aborts remaining in-flight tasks once PROXY_POOL_MAX
/// working proxies are found, avoiding wasted network/CPU.
async fn test_proxies_concurrently(candidates: &[String]) -> Vec<String> {
    use tokio::task::JoinSet;

    let mut join_set: JoinSet<Option<String>> = JoinSet::new();
    let mut working: Vec<String> = Vec::new();

    for proxy_url in candidates.iter().take(200) {
        // Stop spawning if we already have enough working proxies.
        if working.len() >= PROXY_POOL_MAX {
            break;
        }
        let proxy = proxy_url.clone();
        join_set.spawn(async move {
            if test_single_proxy(&proxy).await {
                Some(proxy)
            } else {
                None
            }
        });
        // Throttle: don't have more than PROXY_TEST_CONCURRENCY in flight.
        while join_set.len() >= PROXY_TEST_CONCURRENCY {
            if let Some(res) = join_set.join_next().await
                && let Ok(Some(proxy)) = res
            {
                working.push(proxy);
                if working.len() >= PROXY_POOL_MAX {
                    join_set.abort_all();
                    return working;
                }
            }
        }
    }

    // Drain remaining tasks (if we didn't early-exit above).
    while let Some(res) = join_set.join_next().await
        && let Ok(Some(proxy)) = res
    {
        working.push(proxy);
        if working.len() >= PROXY_POOL_MAX {
            join_set.abort_all();
            break;
        }
    }
    working
}

/// Tests a single proxy by fetching Reddit's about.json for r/Metal.
async fn test_single_proxy(proxy_url: &str) -> bool {
    let proxy = match reqwest::Proxy::all(proxy_url) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let client = match reqwest::Client::builder()
        .proxy(proxy)
        .connect_timeout(PROXY_TEST_TIMEOUT)
        .timeout(PROXY_TEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let url = "https://www.reddit.com/r/Metal/about.json";
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            // Verify it's actually JSON (not a block page).
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            content_type.contains("json")
        }
        _ => false,
    }
}

/// Checks if a line from a proxy list looks like a valid `host:port` entry.
fn is_valid_proxy_line(line: &str) -> bool {
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    // Expect format: host:port (e.g. "192.0.2.1:8080")
    let mut parts = line.rsplitn(2, ':');
    let port_str = parts.next().unwrap_or("");
    let host = parts.next().unwrap_or("");
    let port: u16 = match port_str.parse() {
        Ok(p) => p,
        Err(_) => return false,
    };
    if port == 0 {
        return false;
    }
    // Host part should not contain spaces.
    !host.contains(' ')
}

#[derive(Debug, FromRow)]
struct DueConnectionRow {
    id: Uuid,
    workspace_id: Uuid,
    platform: String,
    provider_account_id: String,
}

#[derive(Clone, Debug)]
struct DueConnection {
    id: Uuid,
    workspace_id: Uuid,
    platform: String,
    provider_account_id: String,
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

#[derive(Debug, Deserialize)]
struct RedditAboutResponse {
    data: RedditAboutData,
}

#[derive(Debug, Deserialize)]
struct RedditAboutData {
    subscribers: i64,
    display_name: Option<String>,
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
    fn reddit_about_response_parses_subscribers() {
        let json = br#"{"kind":"t5","data":{"subscribers":1983452,"display_name":"Metal"}}"#;
        let response: RedditAboutResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(response.data.subscribers, 1983452);
        assert_eq!(response.data.display_name.as_deref(), Some("Metal"));
    }

    #[test]
    fn reddit_about_response_without_display_name() {
        let json = br#"{"kind":"t5","data":{"subscribers":42}}"#;
        let response: RedditAboutResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(response.data.subscribers, 42);
        assert!(response.data.display_name.is_none());
    }

    #[test]
    fn is_valid_proxy_line_accepts_host_port() {
        assert!(is_valid_proxy_line("192.0.2.1:8080"));
        assert!(is_valid_proxy_line("198.51.100.1:3128"));
        assert!(is_valid_proxy_line("example.com:80"));
    }

    #[test]
    fn is_valid_proxy_line_rejects_garbage() {
        assert!(!is_valid_proxy_line(""));
        assert!(!is_valid_proxy_line("# comment"));
        assert!(!is_valid_proxy_line("not a proxy"));
        assert!(!is_valid_proxy_line("no-port-here"));
        assert!(!is_valid_proxy_line("192.0.2.1:0"));
        assert!(!is_valid_proxy_line("192.0.2.1:999999"));
    }

    #[test]
    fn reddit_proxy_pool_starts_empty() {
        let pool = RedditProxyPool::new();
        assert!(pool.working.is_empty());
        assert!(pool.last_refresh.is_none());
    }

    #[test]
    fn reddit_proxy_pool_mark_failed_evicts() {
        let mut pool = RedditProxyPool::new();
        pool.working = vec![
            "http://192.0.2.1:8080".to_string(),
            "http://198.51.100.1:8080".to_string(),
        ];
        pool.mark_failed("http://192.0.2.1:8080");
        assert_eq!(pool.working.len(), 1);
        assert_eq!(pool.working[0], "http://198.51.100.1:8080");
    }

    #[test]
    fn reddit_proxy_pool_mark_failed_unknown_is_noop() {
        let mut pool = RedditProxyPool::new();
        pool.working = vec!["http://192.0.2.1:8080".to_string()];
        pool.mark_failed("http://203.0.113.1:8080");
        assert_eq!(pool.working.len(), 1);
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
}
