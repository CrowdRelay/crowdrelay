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
const SYNCED_PLATFORMS: &[&str] = &["youtube", "spotify", "reddit"];

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
    #[error("no spotify credentials configured")]
    NoSpotifyCredentials,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct GrowthMetricSyncWorker {
    pool: PgPool,
    http_client: reqwest::Client,
    youtube_api_key: Option<String>,
    spotify_client_id: Option<String>,
    spotify_client_secret: Option<String>,
    /// Static proxy override. If set, the worker uses this single proxy
    /// for all Reddit requests instead of the free proxy pool.
    reddit_static_proxy: Option<String>,
    /// Rotating free-proxy pool for Reddit. Lazily populated on first
    /// Reddit sync and refreshed when exhausted or stale.
    reddit_proxy_pool: Arc<Mutex<RedditProxyPool>>,
    operation_timeout: Duration,
}

impl GrowthMetricSyncWorker {
    /// Creates a new worker. Returns `Ok(None)` if no platform is configured
    /// (no YouTube API key and no Spotify credentials) — the caller should
    /// not spawn the worker in that case. Reddit needs no credentials, so
    /// the worker is enabled as long as any other platform is configured.
    pub fn new(
        pool: PgPool,
        youtube_api_key: Option<String>,
        spotify_client_id: Option<String>,
        spotify_client_secret: Option<String>,
        reddit_proxy_url: Option<String>,
        operation_timeout: Duration,
    ) -> Result<Option<Self>, GrowthMetricSyncError> {
        if youtube_api_key.is_none()
            && (spotify_client_id.is_none() || spotify_client_secret.is_none())
        {
            tracing::info!(
                "growth metric sync disabled: no YouTube API key or Spotify credentials"
            );
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
            spotify_client_id,
            spotify_client_secret,
            reddit_static_proxy: reddit_proxy_url,
            reddit_proxy_pool: Arc::new(Mutex::new(RedditProxyPool::new())),
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
                if let Err(error) = self.sync_connection(&conn).await {
                    tracing::warn!(
                        connection_id = %conn.id,
                        platform = %conn.platform,
                        error = %error,
                        "growth metric sync failed for connection"
                    );
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

    /// Spotify: fetch artist follower count via Web API (client credentials).
    async fn sync_spotify(&self, conn: &DueConnection) -> Result<(), GrowthMetricSyncError> {
        let client_id = self
            .spotify_client_id
            .as_ref()
            .ok_or(GrowthMetricSyncError::NoSpotifyCredentials)?;
        let client_secret = self
            .spotify_client_secret
            .as_ref()
            .ok_or(GrowthMetricSyncError::NoSpotifyCredentials)?;
        let artist_id = &conn.provider_account_id;

        // Client credentials flow: get an access token.
        let token_response = self
            .http_client
            .post("https://accounts.spotify.com/api/token")
            .form(&[("grant_type", "client_credentials")])
            .basic_auth(client_id, Some(client_secret))
            .send()
            .await?;
        if !token_response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Spotify token endpoint returned HTTP {}",
                token_response.status()
            )));
        }
        let token: SpotifyTokenResponse = token_response.json().await?;

        // Fetch artist info.
        let artist_url = format!("https://api.spotify.com/v1/artists/{artist_id}");
        let response = self
            .http_client
            .get(&artist_url)
            .bearer_auth(&token.access_token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(GrowthMetricSyncError::ProviderApi(format!(
                "Spotify API returned HTTP {}",
                response.status()
            )));
        }
        let artist: SpotifyArtistResponse = response.json().await?;
        let display_name = &artist.name;

        // Spotify deprecated the `followers` field in the public Web API
        // (client credentials flow). The field is now omitted from all
        // responses. We handle both cases: if followers is present, record
        // it; if not, log a warning and skip — the series stays live from
        // the last manual/operator-inserted data point until Spotify
        // restores the field or we switch to user-level OAuth.
        let follower_count = artist.followers.as_ref().map(|f| f.total);

        if let Some(count) = follower_count {
            record_metric_point(
                &self.pool,
                conn.workspace_id,
                conn.id,
                "spotify",
                "followers",
                &format!("Spotify followers — {display_name}"),
                count,
                OffsetDateTime::now_utc(),
            )
            .await?;

            tracing::info!(
                connection_id = %conn.id,
                artist_id = %artist_id,
                artist = %display_name,
                followers = count,
                "spotify follower count recorded"
            );
        } else {
            tracing::warn!(
                connection_id = %conn.id,
                artist_id = %artist_id,
                artist = %display_name,
                "spotify API returned no followers field — Spotify deprecated \
                 follower counts in the public Web API. Series will go stale \
                 unless data is inserted manually or via user-level OAuth."
            );
        }
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
            tested = candidates.len(),
            "reddit proxy pool: refresh complete"
        );

        self.working = working;
        self.next_idx.store(0, Ordering::Relaxed);
        self.last_refresh = Some(Instant::now());
    }

    /// Fetches proxy lists from all public sources and deduplicates.
    async fn fetch_proxy_candidates(&self, client: &reqwest::Client) -> Vec<String> {
        let mut all: Vec<String> = Vec::new();
        for source in PROXY_SOURCES {
            match client
                .get(*source)
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let text = match resp.text().await {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    for line in text.lines() {
                        let line = line.trim();
                        if is_valid_proxy_line(line) {
                            all.push(format!("http://{line}"));
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, source = *source, "proxy source fetch failed");
                }
                _ => {}
            }
        }
        all.sort();
        all.dedup();
        all
    }
}

/// Tests proxies concurrently against Reddit, returns the ones that work.
async fn test_proxies_concurrently(candidates: &[String]) -> Vec<String> {
    use tokio::task::JoinSet;

    let mut join_set = JoinSet::new();
    for proxy_url in candidates.iter().take(200) {
        let proxy = proxy_url.clone();
        join_set.spawn(async move {
            if test_single_proxy(&proxy).await {
                Some(proxy)
            } else {
                None
            }
        });
        // Throttle: don't spawn more than PROXY_TEST_CONCURRENCY at once.
        while join_set.len() >= PROXY_TEST_CONCURRENCY {
            if let Some(Ok(Some(_p))) = join_set.join_next().await {
                // Early exit if we have enough working proxies.
                // (We still need to collect from the JoinSet later.)
                // We can't break here — just let it run.
            }
        }
    }

    let mut working: Vec<String> = Vec::new();
    while let Some(res) = join_set.join_next().await {
        if let Ok(Some(proxy)) = res {
            working.push(proxy);
            if working.len() >= PROXY_POOL_MAX {
                break;
            }
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

// --- Spotify response types ---

#[derive(Debug, Deserialize)]
struct SpotifyTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyArtistResponse {
    name: String,
    #[serde(default)]
    followers: Option<SpotifyFollowers>,
}

#[derive(Debug, Deserialize)]
struct SpotifyFollowers {
    total: i64,
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
    fn spotify_artist_response_parses_followers() {
        let json = br#"{"name":"Virya","followers":{"total":12345},"id":"6bbW0jOKAWJWm3h6CTWaAS"}"#;
        let artist: SpotifyArtistResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(artist.name, "Virya");
        assert_eq!(artist.followers.as_ref().unwrap().total, 12345);
    }

    #[test]
    fn spotify_artist_response_parses_without_followers() {
        // Spotify deprecated the followers field in the public Web API.
        // The response may omit it entirely — the struct must still parse.
        let json = br#"{"name":"Virya","id":"6bbW0jOKAWJWm3h6CTWaAS","type":"artist","uri":"spotify:artist:6bbW0jOKAWJWm3h6CTWaAS"}"#;
        let artist: SpotifyArtistResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(artist.name, "Virya");
        assert!(artist.followers.is_none());
    }

    #[test]
    fn spotify_token_response_parses_access_token() {
        let json = br#"{"access_token":"BQxyz123","token_type":"Bearer","expires_in":3600}"#;
        let token: SpotifyTokenResponse = serde_json::from_slice(json).unwrap();
        assert_eq!(token.access_token, "BQxyz123");
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
}
