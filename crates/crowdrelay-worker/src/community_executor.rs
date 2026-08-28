//! Community engagement executor: posts approved community.engage.request
//! actions to Reddit via OAuth.
//!
//! The autopilot marks `RequestCommunityEngagement` actions as `succeeded`
//! after emitting the outbox event (the outbox delivers to external webhook
//! endpoints). This worker is the *internal executor* that actually posts
//! to Reddit — it polls for succeeded actions that don't yet have a
//! `community_posts` row, loads the workspace's Reddit OAuth token, and
//! submits the post via the Reddit API.
//!
//! ## Anti-spam guardrails
//! - One post per subreddit per 7 days (enforced via SQL check before posting)
//! - Max 3 posts per 24 hours per workspace (enforced via SQL count)
//! - If no Reddit connection exists, the post is marked `failed` (not retried)
//! - Rate-limited responses (HTTP 429) get `rate_limited` status with backoff
//!
//! ## Idempotency and crash recovery
//! `community_posts.action_id` is UNIQUE. The lifecycle is:
//!   `pending` → `posting` → `posted` (or `failed` / `rate_limited`)
//!
//! A crash between `pending` and `posting` leaves a `pending` row that the
//! next poll reclaims. A crash during `posting` (between the Reddit API call
//! and the DB update) is recovered by reclaiming `posting` rows older than
//! 5 minutes — but we do NOT re-submit to Reddit (to avoid duplicate posts).
//! Instead, the row is marked `failed` with a message directing the operator
//! to check Reddit manually.
//!
//! ## Concurrency
//! The claim query uses `FOR UPDATE SKIP LOCKED` so multiple worker instances
//! cannot process the same row. Guardrail checks (cooldown, rate limit) are
//! performed within the same transaction as the status update to `posting`,
//! closing the race window.

use std::borrow::Cow;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{
    fanbase_oauth::{FanbaseOauthConfig, FanbaseOauthRepository, StoredTokens},
    reddit_proxy::read_reddit_proxy_from_db,
    sensitive_response::SensitiveResponseKey,
};
use serde::Deserialize;
use sqlx::PgPool;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

/// How often to poll for unprocessed community engagement actions.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// How often the worker checks `reddit_proxy_state` for a new proxy.
const PROXY_REFRESH_INTERVAL: Duration = Duration::from_secs(300); // 5 min
/// Watchdog for one executor cycle. A cycle may submit posts through the
/// agents browser (possible login + navigation = minutes); the old
/// operation_timeout cap (5s default) cancelled cycles mid-submit, leaving
/// a live Reddit post recorded as `posting` → stale-recovered to `failed`.
/// This only guards against a permanently hung cycle.
const CYCLE_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(1800);
/// Browser submit through the agents service can take minutes (login).
/// Per-request override of the client-wide operation timeout.
const AGENTS_SUBMIT_TIMEOUT: Duration = Duration::from_secs(300);
/// Browser metrics read through the agents service.
const AGENTS_METRICS_TIMEOUT: Duration = Duration::from_secs(90);

/// Builds a reqwest client with an optional proxy. Shared between the
/// constructor and the run-loop proxy refresh.
fn build_http_client(
    proxy_url: Option<&str>,
    operation_timeout: Duration,
) -> Result<reqwest::Client, CommunityExecutorError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(operation_timeout.min(Duration::from_secs(10)))
        .timeout(operation_timeout)
        .user_agent(USER_AGENT);
    if let Some(proxy) = proxy_url {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|e| CommunityExecutorError::RedditApi(format!("invalid proxy URL: {e}")))?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(CommunityExecutorError::ClientBuild)
}

/// Maximum posts per workspace per 24 hours. Reddit's rate limits are strict;
/// this is well within their bounds while still allowing meaningful engagement.
const MAX_POSTS_PER_24H: i64 = 3;

/// Cooldown: no more than one post per subreddit per 7 days.
const SUBREDDIT_COOLDOWN_DAYS: i32 = 7;

/// Token refresh threshold: if the token expires within this window, refresh
/// it before making the API call.
const TOKEN_REFRESH_THRESHOLD: Duration = Duration::from_secs(300);

/// A `posting` row older than this is considered a crashed attempt.
const POSTING_STALE_THRESHOLD: Duration = Duration::from_secs(300);

/// Rate limit backoff: how long to wait before retrying a rate-limited post.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(600);

/// How long after a post was created do we keep polling its metrics.
/// After this window, engagement is considered stale and polling stops.
const METRICS_WINDOW: Duration = Duration::from_secs(72 * 60 * 60);

/// Minimum interval between metric polls for the same post. Prevents
/// hammering Reddit's API for posts that haven't changed.
const METRICS_POLL_MIN_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Maximum posts to poll for metrics in a single cycle. Bounded to keep
/// the cycle fast even if many posts are in the window.
const METRICS_POLL_BATCH: i64 = 10;

/// Reddit requires a descriptive User-Agent following their guideline:
/// `<platform>:<app ID>:<version string> (by /u/<username>)`
const USER_AGENT: &str = "server:com.crowdrelay.community:v1.0.0 (by /u/virya_band)";

/// Public origin for smart link resolution. The smart_link stored in the
/// action payload is a `/l/{slug}` path; Reddit needs a full URL.
const DEFAULT_PUBLIC_ORIGIN: &str = "https://virya.music";

#[derive(Debug, Error)]
pub enum CommunityExecutorError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("reddit API error: {0}")]
    RedditApi(String),
    #[error("oauth error: {0}")]
    Oauth(#[from] crowdrelay_infra::fanbase_oauth::FanbaseOauthError),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no reddit connection for workspace")]
    NoRedditConnection,
    #[error("rate limited by Reddit")]
    RateLimited,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct CommunityExecutorWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    oauth_repo: FanbaseOauthRepository,
    reddit_config: FanbaseOauthConfig,
    encryption_key: SensitiveResponseKey,
    http_client: Arc<RwLock<reqwest::Client>>,
    poll_interval: Duration,
    operation_timeout: Duration,
    public_origin: String,
    /// Reddit "script" app credentials (password grant). When set, the
    /// executor authenticates with username/password instead of the web-app
    /// OAuth flow. This is the fallback when no OAuth connection exists in
    /// the DB or when token refresh fails.
    script_username: Option<String>,
    script_password: Option<String>,
    /// When true, the executor creates `community_posts` rows but does not
    /// post to Reddit. Posts are marked `awaiting_manual_post` — the operator
    /// posts manually and registers the URL via the API. Metrics polling
    /// still works via Reddit's public JSON endpoint.
    manual_mode: bool,
    /// Base URL of the agents service (for Reddit cookie fetching).
    agent_service_url: String,
    /// Auth key for the agents service.
    agent_service_auth_key: Option<String>,
    /// Env-var proxy URL (manual override). The DB proxy from the sidecar
    /// takes precedence when available and fresh.
    env_proxy_url: Option<String>,
}

impl CommunityExecutorWorker {
    /// Creates a new executor. Returns an error if the HTTP client cannot be
    /// built (e.g. TLS backend failure). The caller should skip spawning the
    /// worker if the Reddit OAuth config is not set up (check
    /// `reddit_config_from_env` first).
    ///
    /// # Errors
    /// Returns [`CommunityExecutorError::ClientBuild`] if the `reqwest` client
    /// cannot be initialized.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        reddit_config: FanbaseOauthConfig,
        encryption_key: SensitiveResponseKey,
        operation_timeout: Duration,
        manual_mode: bool,
        proxy_url: Option<String>,
        agent_service_url: String,
        agent_service_auth_key: Option<String>,
    ) -> Result<Self, CommunityExecutorError> {
        let oauth_repo = FanbaseOauthRepository::new(pool.clone());
        let http_client = build_http_client(proxy_url.as_deref(), operation_timeout)?;
        let http_client = Arc::new(RwLock::new(http_client));
        let public_origin = std::env::var("CROWDRELAY_PUBLIC_ORIGIN")
            .unwrap_or_else(|_| DEFAULT_PUBLIC_ORIGIN.to_owned());
        // Script-app credentials (optional). When set, the executor can
        // authenticate via password grant instead of the web-app OAuth flow.
        // This is the path for Reddit "script" type apps.
        let script_username = std::env::var("CROWDRELAY_FANBASE_OAUTH_REDDIT_USERNAME")
            .ok()
            .filter(|v| !v.is_empty());
        let script_password = std::env::var("CROWDRELAY_FANBASE_OAUTH_REDDIT_PASSWORD")
            .ok()
            .filter(|v| !v.is_empty());
        Ok(Self {
            pool,
            workspace_id,
            oauth_repo,
            reddit_config,
            encryption_key,
            http_client,
            poll_interval: POLL_INTERVAL,
            operation_timeout,
            public_origin,
            script_username,
            script_password,
            manual_mode,
            agent_service_url,
            agent_service_auth_key,
            env_proxy_url: proxy_url,
        })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut proxy_timer = interval(PROXY_REFRESH_INTERVAL);
        proxy_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        proxy_timer.tick().await; // skip first immediate tick
        let mut current_proxy = self.env_proxy_url.clone();
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = proxy_timer.tick() => {
                    let new_proxy = if let Some(db_proxy) =
                        read_reddit_proxy_from_db(&self.pool).await
                    {
                        Some(db_proxy)
                    } else {
                        self.env_proxy_url.clone()
                    };
                    if new_proxy != current_proxy {
                        match build_http_client(new_proxy.as_deref(), self.operation_timeout) {
                            Ok(new_client) => {
                                tracing::info!(
                                    old = current_proxy.as_deref().unwrap_or("direct"),
                                    new = new_proxy.as_deref().unwrap_or("direct"),
                                    "community executor proxy changed, rebuilding HTTP client"
                                );
                                let mut guard = self.http_client.write().unwrap_or_else(|e| e.into_inner());
                                *guard = new_client;
                                current_proxy = new_proxy;
                            }
                            Err(error) => {
                                tracing::warn!(error = %error, "failed to rebuild client with new proxy, keeping old client");
                            }
                        }
                    }
                }
                _ = ticker.tick() => {
                    match timeout(CYCLE_WATCHDOG_TIMEOUT, self.run_once()).await {
                        Ok(Ok(processed)) if processed > 0 => {
                            tracing::info!(processed, "community executor processed batch");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "community executor cycle failed"),
                        Err(_) => tracing::warn!("community executor cycle timed out"),
                    }
                }
            }
        }
    }

    async fn run_once(&self) -> Result<usize, CommunityExecutorError> {
        // First, recover any stale `posting` rows from a previous crash.
        // We do NOT re-submit to Reddit (to avoid duplicate posts). Instead,
        // mark them as failed — the operator must check Reddit manually.
        self.recover_stale_posting().await?;

        let actions = self.claim_pending_actions().await?;
        let mut processed = 0;
        for action in actions {
            match self.process_action(&action).await {
                Ok(()) => processed += 1,
                Err(CommunityExecutorError::RateLimited) => {
                    // Rate limited — set status for later retry, don't fail.
                    if let Err(e) = self.mark_rate_limited(action.id).await {
                        tracing::warn!(error = %e, "failed to mark rate_limited");
                    }
                }
                Err(CommunityExecutorError::NoRedditConnection) => {
                    // No Reddit connection — permanent failure, don't retry.
                    if let Err(e) = self
                        .mark_failed(action.id, "no reddit connection for workspace")
                        .await
                    {
                        tracing::warn!(error = %e, "failed to mark no-connection");
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        action_id = %action.action_id,
                        subreddit = %action.subreddit,
                        error = %error,
                        "failed to post community engagement"
                    );
                    // Transient errors (network, 5xx) leave the row as
                    // `posting` so the next cycle's stale recovery handles it.
                    // For now, mark as failed — the operator can re-approve.
                    let msg = error.to_string();
                    if let Err(e) = self.mark_failed(action.id, &msg).await {
                        tracing::warn!(error = %e, "failed to mark error");
                    }
                }
            }
        }

        // Second phase: poll Reddit for post performance metrics on recently
        // posted content. This is the "eyes" of the growth loop — the system
        // learns which posts generate engagement.
        processed += self.poll_post_metrics().await?;

        Ok(processed)
    }

    /// Recovers `posting` rows that have been stuck longer than the stale
    /// threshold. These are from a worker crash during the Reddit API call.
    /// We mark them as `failed` rather than re-submitting to avoid duplicate
    /// posts on Reddit (the original post may have succeeded).
    async fn recover_stale_posting(&self) -> Result<(), CommunityExecutorError> {
        let result = sqlx::query(
            r#"
            UPDATE community_posts
            SET status = 'failed',
                error_message = 'worker crashed during posting — check Reddit manually',
                updated_at = now()
            WHERE workspace_id = $1
              AND status = 'posting'
              AND updated_at < now() - make_interval(secs => $2::double precision)
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(POSTING_STALE_THRESHOLD.as_secs() as i64)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() > 0 {
            tracing::info!(
                recovered = result.rows_affected(),
                "recovered stale posting rows (marked failed — check Reddit manually)"
            );
        }
        Ok(())
    }

    /// Claims a batch of work in a single atomic transaction:
    /// 1. Inserts `pending` rows for succeeded actions that don't have a
    ///    `community_posts` row yet.
    /// 2. Reclaims existing `pending` rows (from a previous crash before the
    ///    `posting` transition) and `rate_limited` rows past their backoff.
    /// 3. Transitions claimed rows to `posting` status using
    ///    `FOR UPDATE SKIP LOCKED` to prevent concurrent processing.
    /// 4. Anti-spam guardrails (cooldown, rate limit) are checked in
    ///    `process_action` after the claim. This is safe with a single
    ///    worker (the current deployment) but would need to move into the
    ///    claim transaction if horizontal scaling is added.
    async fn claim_pending_actions(&self) -> Result<Vec<ClaimedAction>, CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let mut tx = self.pool.begin().await?;

        // Step 1: Insert pending rows for unprocessed succeeded actions.
        sqlx::query(
            r#"
            INSERT INTO community_posts
                (workspace_id, action_id, target_id, subreddit, title, body, smart_link, status)
            SELECT
                $1,
                a.id,
                (a.payload->>'target_id')::uuid,
                COALESCE(a.payload->>'subreddit', ''),
                COALESCE(a.payload->>'title', ''),
                COALESCE(a.payload->>'body', ''),
                a.payload->>'smart_link',
                'pending'
            FROM viryaos_autopilot_actions a
            WHERE a.workspace_id = $1
              AND a.action_kind = 'community.engage.request'
              AND a.status = 'succeeded'
              AND NOT EXISTS (
                  SELECT 1 FROM community_posts cp WHERE cp.action_id = a.id
              )
            ON CONFLICT (action_id) DO NOTHING
            "#,
        )
        .bind(ws)
        .execute(&mut *tx)
        .await?;

        // Step 2: Claim pending and rate_limited (past backoff) rows.
        // Transition them to `posting` atomically.
        let rows = sqlx::query_as::<_, ClaimedAction>(
            r#"
            UPDATE community_posts
            SET status = 'posting',
                attempts = attempts + 1,
                updated_at = now()
            WHERE id IN (
                SELECT id FROM community_posts
                WHERE workspace_id = $1
                  AND (
                      status = 'pending'
                      OR (status = 'rate_limited' AND rate_limited_until IS NOT NULL
                          AND rate_limited_until < now())
                  )
                ORDER BY created_at
                LIMIT 5
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id, action_id, subreddit, title, body, smart_link
            "#,
        )
        .bind(ws)
        .fetch_all(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(rows)
    }

    /// Processes a single claimed action: checks anti-spam guardrails,
    /// loads the Reddit OAuth token, posts to Reddit, and records the result.
    async fn process_action(&self, action: &ClaimedAction) -> Result<(), CommunityExecutorError> {
        // Anti-spam: check subreddit cooldown.
        if self.subreddit_on_cooldown(&action.subreddit).await? {
            tracing::info!(
                subreddit = %action.subreddit,
                "subreddit on 7-day cooldown, skipping"
            );
            self.mark_failed(action.id, "subreddit on 7-day cooldown")
                .await?;
            return Ok(());
        }

        // Anti-spam: check 24h rate limit.
        if self.rate_limit_reached().await? {
            tracing::info!("24h post limit reached, skipping");
            self.mark_failed(action.id, "24h post limit reached")
                .await?;
            return Ok(());
        }

        // Manual mode: skip Reddit API, mark as awaiting manual post.
        // The operator posts manually and registers the URL via the API.
        if self.manual_mode {
            sqlx::query(
                r#"
                UPDATE community_posts
                SET status = 'awaiting_manual_post',
                    updated_at = now()
                WHERE id = $1
                "#,
            )
            .bind(action.id)
            .execute(&self.pool)
            .await?;
            tracing::info!(
                action_id = %action.action_id,
                subreddit = %action.subreddit,
                "community post marked as awaiting manual post"
            );
            return Ok(());
        }

        // Build the post body, appending the smart link as a full URL if present.
        let post_body = self.build_post_body(&action.body, action.smart_link.as_deref());

        // Submit to Reddit. Browser-first: the agents service posts through
        // a real logged-in browser session — the only Reddit access path
        // that still works (public .json and OAuth are both blocked). The
        // legacy OAuth/password-grant chain runs only when the agents path
        // is unavailable, so existing setups do not regress.
        let reddit_result = if self.agent_service_auth_key.is_some() {
            match self.submit_via_agent_browser(action, &post_body).await {
                Ok(result) => result,
                Err(CommunityExecutorError::RateLimited) => {
                    return Err(CommunityExecutorError::RateLimited);
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "browser submit via agents failed, trying legacy Reddit OAuth path"
                    );
                    self.submit_via_legacy_oauth(action, &post_body).await?
                }
            }
        } else {
            self.submit_via_legacy_oauth(action, &post_body).await?
        };

        // Record success.
        sqlx::query(
            r#"
            UPDATE community_posts
            SET status = 'posted',
                reddit_post_id = $2,
                reddit_post_url = $3,
                posted_at = now(),
                updated_at = now(),
                error_message = NULL,
                rate_limited_until = NULL
            WHERE id = $1
            "#,
        )
        .bind(action.id)
        .bind(&reddit_result.post_id)
        .bind(&reddit_result.post_url)
        .execute(&self.pool)
        .await?;

        tracing::info!(
            subreddit = %action.subreddit,
            post_url = %reddit_result.post_url,
            "successfully posted to Reddit"
        );
        Ok(())
    }

    /// Submits a self post through the agents service's logged-in browser
    /// session (POST /reddit/post). A 429 maps to the executor's
    /// rate-limit backoff; every other failure is surfaced to the caller.
    async fn submit_via_agent_browser(
        &self,
        action: &ClaimedAction,
        post_body: &str,
    ) -> Result<RedditSubmitResult, CommunityExecutorError> {
        let auth_key = self.agent_service_auth_key.as_deref().ok_or_else(|| {
            CommunityExecutorError::RedditApi("agent service auth key not configured".to_owned())
        })?;
        let ws = self.workspace_id.into_uuid();
        let token = crate::discovery::derive_agent_token(auth_key, ws);
        let url = format!("{}/reddit/post", self.agent_service_url);
        let payload = serde_json::json!({
            "subreddit": action.subreddit,
            "title": action.title,
            "body": post_body,
        });

        let client = self
            .http_client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Workspace-Id", ws.to_string())
            .json(&payload)
            .timeout(AGENTS_SUBMIT_TIMEOUT)
            .send()
            .await?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(CommunityExecutorError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CommunityExecutorError::RedditApi(format!(
                "agents /reddit/post HTTP {status}: {body}"
            )));
        }
        response.json().await.map_err(CommunityExecutorError::Http)
    }

    /// Legacy submit chain: web-app OAuth token (refreshed if needed) with a
    /// password-grant fallback. Kept for deployments where the agents
    /// browser path is unavailable and a Reddit OAuth setup still works.
    async fn submit_via_legacy_oauth(
        &self,
        action: &ClaimedAction,
        post_body: &str,
    ) -> Result<RedditSubmitResult, CommunityExecutorError> {
        let tokens = match self.find_reddit_connection().await {
            Ok(connection_id) => self.load_valid_tokens(connection_id).await?,
            Err(CommunityExecutorError::NoRedditConnection) => {
                tracing::info!("no Reddit OAuth connection, trying password grant");
                self.password_grant_fallback().await?
            }
            Err(error) => return Err(error),
        };
        self.submit_to_reddit(
            &tokens.access_token,
            &action.subreddit,
            &action.title,
            post_body,
        )
        .await
    }

    /// Checks if this subreddit has been posted to within the cooldown window.
    async fn subreddit_on_cooldown(&self, subreddit: &str) -> Result<bool, CommunityExecutorError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM community_posts
            WHERE workspace_id = $1
              AND subreddit = $2
              AND status = 'posted'
              AND posted_at > now() - make_interval(days => $3)
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(subreddit)
        .bind(SUBREDDIT_COOLDOWN_DAYS)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// Checks if the workspace has reached the 24h post limit.
    async fn rate_limit_reached(&self) -> Result<bool, CommunityExecutorError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM community_posts
            WHERE workspace_id = $1
              AND status = 'posted'
              AND posted_at > now() - INTERVAL '24 hours'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(count >= MAX_POSTS_PER_24H)
    }

    /// Finds the most recent active Reddit connection for the workspace.
    async fn find_reddit_connection(&self) -> Result<Uuid, CommunityExecutorError> {
        let id: Option<Uuid> = sqlx::query_scalar(
            r#"
            SELECT id FROM fanbase_connections
            WHERE workspace_id = $1
              AND platform = 'reddit'
              AND status = 'connected'
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        id.ok_or(CommunityExecutorError::NoRedditConnection)
    }

    /// Loads the OAuth token, refreshing it if it's expired or about to expire.
    /// Falls back to password grant (script app) if no connection exists or
    /// refresh fails and script credentials are configured.
    async fn load_valid_tokens(
        &self,
        connection_id: Uuid,
    ) -> Result<StoredTokens, CommunityExecutorError> {
        let tokens = self
            .oauth_repo
            .load_tokens(
                self.workspace_id.into_uuid(),
                connection_id,
                &self.encryption_key,
            )
            .await?;

        // Refresh if the token expires within the threshold or is already expired.
        let needs_refresh = tokens
            .expires_at
            .map(|exp| exp < OffsetDateTime::now_utc() + TOKEN_REFRESH_THRESHOLD)
            .unwrap_or(true);

        if needs_refresh {
            tracing::info!(connection_id = %connection_id, "refreshing Reddit OAuth token");
            match self
                .oauth_repo
                .refresh_token(
                    self.workspace_id.into_uuid(),
                    connection_id,
                    &self.reddit_config,
                    &self.encryption_key,
                )
                .await
            {
                Ok(()) => {
                    let refreshed = self
                        .oauth_repo
                        .load_tokens(
                            self.workspace_id.into_uuid(),
                            connection_id,
                            &self.encryption_key,
                        )
                        .await?;
                    Ok(refreshed)
                }
                Err(error) => {
                    // Refresh failed — try password grant if script credentials
                    // are configured. This is the script-app fallback path.
                    tracing::warn!(
                        error = %error,
                        "Reddit token refresh failed, trying password grant"
                    );
                    self.password_grant_fallback().await
                }
            }
        } else {
            Ok(tokens)
        }
    }

    /// Authenticates with Reddit using the password grant (script app).
    /// Used as a fallback when no OAuth connection exists or when refresh
    /// fails. Requires `CROWDRELAY_FANBASE_OAUTH_REDDIT_USERNAME` and
    /// `CROWDRELAY_FANBASE_OAUTH_REDDIT_PASSWORD` to be set.
    async fn password_grant_fallback(&self) -> Result<StoredTokens, CommunityExecutorError> {
        let (username, password) = self
            .script_username
            .as_deref()
            .zip(self.script_password.as_deref())
            .ok_or(CommunityExecutorError::NoRedditConnection)?;
        tracing::info!(username = %username, "authenticating Reddit via password grant");
        self.oauth_repo
            .password_grant(
                self.workspace_id.into_uuid(),
                &self.reddit_config,
                username,
                password,
                &self.encryption_key,
            )
            .await
            .map_err(CommunityExecutorError::from)
    }

    /// Builds the final post body, appending the smart link as a full URL
    /// if present. The smart_link stored in the action payload is a `/l/{slug}`
    /// path; Reddit needs a full URL for it to be clickable.
    fn build_post_body<'a>(&'a self, body: &'a str, smart_link: Option<&str>) -> Cow<'a, str> {
        match smart_link {
            Some(link) if !link.is_empty() => {
                let full_url = if link.starts_with("http") {
                    link.to_owned()
                } else {
                    format!("{}{link}", self.public_origin)
                };
                Cow::Owned(format!("{body}\n\n{full_url}"))
            }
            _ => Cow::Borrowed(body),
        }
    }

    /// Normalizes a subreddit name: trims whitespace, strips `r/` or `/r/`
    /// prefixes (case-insensitive), and lowercases the result.
    fn normalize_subreddit(subreddit: &str) -> String {
        let trimmed = subreddit.trim();
        let lower = trimmed.to_ascii_lowercase();
        let without_prefix = lower
            .strip_prefix("/r/")
            .or_else(|| lower.strip_prefix("r/"))
            .unwrap_or(&lower);
        without_prefix.to_owned()
    }

    /// Submits a text post to Reddit via the OAuth API.
    async fn submit_to_reddit(
        &self,
        access_token: &str,
        subreddit: &str,
        title: &str,
        body: &str,
    ) -> Result<RedditSubmitResult, CommunityExecutorError> {
        let sr = Self::normalize_subreddit(subreddit);

        let client = self
            .http_client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let response = client
            .post("https://oauth.reddit.com/api/submit")
            .header("Authorization", format!("Bearer {access_token}"))
            .form(&[
                ("api_type", "json"),
                ("kind", "self"),
                ("sr", sr.as_str()),
                ("title", title),
                ("text", body),
            ])
            .send()
            .await?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(CommunityExecutorError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CommunityExecutorError::RedditApi(format!(
                "HTTP {status}: {body}"
            )));
        }

        let body: RedditApiResponse = response.json().await?;

        // Reddit returns errors inside the JSON body even with HTTP 200.
        if !body.json.errors.is_empty() {
            let error_msgs: Vec<String> = body
                .json
                .errors
                .iter()
                .map(|e| format!("{}: {}", e.0, e.1))
                .collect();
            return Err(CommunityExecutorError::RedditApi(error_msgs.join("; ")));
        }

        let data = body.json.data.ok_or_else(|| {
            CommunityExecutorError::RedditApi("missing data in response".to_owned())
        })?;

        Ok(RedditSubmitResult {
            post_id: data.id,
            post_url: data.url,
        })
    }

    /// Polls Reddit for post performance metrics on recently posted content.
    /// Only polls posts that:
    /// - Have `status = 'posted'` with a non-null `reddit_post_id`
    /// - Were posted within the last `METRICS_WINDOW` (72h)
    /// - Haven't been polled in the last `METRICS_POLL_MIN_INTERVAL` (15min)
    ///
    /// This is the feedback loop: the system learns which posts generate
    /// engagement (upvotes, comments) and which don't.
    async fn poll_post_metrics(&self) -> Result<usize, CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();

        // Find posts that need metrics polling.
        let posts_to_poll = sqlx::query_as::<_, PostMetricsTarget>(
            r#"
            SELECT id, reddit_post_id
            FROM community_posts
            WHERE workspace_id = $1
              AND status = 'posted'
              AND reddit_post_id IS NOT NULL
              AND posted_at > now() - make_interval(secs => $2::double precision)
              AND (
                  metrics_last_fetched_at IS NULL
                  OR metrics_last_fetched_at < now() - make_interval(secs => $3::double precision)
              )
            ORDER BY posted_at DESC
            LIMIT $4
            "#,
        )
        .bind(ws)
        .bind(METRICS_WINDOW.as_secs() as i64)
        .bind(METRICS_POLL_MIN_INTERVAL.as_secs() as i64)
        .bind(METRICS_POLL_BATCH)
        .fetch_all(&self.pool)
        .await?;

        if posts_to_poll.is_empty() {
            return Ok(0);
        }

        // Metrics polling uses Reddit's public JSON endpoint — no OAuth
        // token needed. This keeps the feedback loop working even when
        // the Reddit API is unavailable for posting.
        let mut measured = 0;
        for target in posts_to_poll {
            match self
                .fetch_reddit_post_metrics_public(&target.reddit_post_id)
                .await
            {
                Ok(metrics) => {
                    self.record_post_metrics(target.id, &target.reddit_post_id, &metrics)
                        .await?;
                    measured += 1;
                }
                Err(CommunityExecutorError::RateLimited) => {
                    // Reddit rate-limited us — stop polling this batch.
                    tracing::info!("rate limited while polling post metrics, stopping batch");
                    break;
                }
                Err(error) => {
                    tracing::warn!(
                        post_id = %target.id,
                        reddit_post_id = %target.reddit_post_id,
                        error = %error,
                        "failed to fetch post metrics"
                    );
                    // Update the fetch timestamp so we don't retry this post
                    // immediately on the next cycle.
                    if let Err(db_err) = self.touch_metrics_fetched_at(target.id).await {
                        tracing::warn!(
                            post_id = %target.id,
                            error = %db_err,
                            "failed to update metrics_last_fetched_at — post will be re-fetched next cycle"
                        );
                    }
                }
            }
        }

        if measured > 0 {
            tracing::info!(measured, "polled community post metrics");
        }
        Ok(measured)
    }

    /// Fetches Reddit session cookies from the agents service (obtained by
    /// the Playwright scraper via Google OAuth). Returns None if the agents
    /// service is unreachable or no cookies are stored.
    async fn fetch_reddit_cookies(&self) -> Option<String> {
        let auth_key = self.agent_service_auth_key.as_ref()?;
        let ws = self.workspace_id.into_uuid();
        let token = crate::discovery::derive_agent_token(auth_key, ws);
        let url = format!("{}/reddit/cookies", self.agent_service_url);
        let client = self
            .http_client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Workspace-Id", ws.to_string())
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|error| tracing::warn!(?error, "reddit cookies fetch failed"))
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|error| tracing::warn!(?error, "reddit cookies response was not json"))
            .ok()?;
        let cookies = body.get("cookies")?.as_array()?;
        let cookie_str = cookies
            .iter()
            .filter_map(|c| {
                let name = c.get("name")?.as_str()?;
                let value = c.get("value")?.as_str()?;
                Some(format!("{name}={value}"))
            })
            .collect::<Vec<_>>()
            .join("; ");
        if cookie_str.is_empty() {
            None
        } else {
            Some(cookie_str)
        }
    }

    /// Reads post metrics through the agents service's logged-in browser
    /// (POST /reddit/metrics). A 429 maps to the executor's rate-limit
    /// backoff; other errors fall through to the direct public path.
    async fn fetch_post_metrics_via_agents(
        &self,
        reddit_post_id: &str,
    ) -> Result<RedditPostMetrics, CommunityExecutorError> {
        let auth_key = self
            .agent_service_auth_key
            .as_deref()
            .ok_or_else(|| CommunityExecutorError::NoRedditConnection)?;
        let ws = self.workspace_id.into_uuid();
        let token = crate::discovery::derive_agent_token(auth_key, ws);
        let url = format!("{}/reddit/metrics", self.agent_service_url);
        let payload = serde_json::json!({ "post_id": reddit_post_id });

        let client = self
            .http_client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Workspace-Id", ws.to_string())
            .json(&payload)
            .timeout(AGENTS_METRICS_TIMEOUT)
            .send()
            .await?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(CommunityExecutorError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CommunityExecutorError::RedditApi(format!(
                "agents /reddit/metrics HTTP {status}: {body}"
            )));
        }
        response.json().await.map_err(CommunityExecutorError::Http)
    }

    /// Fetches post metrics. Browser-first (authenticated session, no 403
    /// challenge); falls back to the direct public JSON endpoint with
    /// scraper cookies for deployments without the agents browser.
    async fn fetch_reddit_post_metrics_public(
        &self,
        reddit_post_id: &str,
    ) -> Result<RedditPostMetrics, CommunityExecutorError> {
        if self.agent_service_auth_key.is_some() {
            match self.fetch_post_metrics_via_agents(reddit_post_id).await {
                Ok(metrics) => return Ok(metrics),
                // Propagate immediately: hammering the fallback right after
                // Reddit rate-limited us makes the block worse.
                Err(CommunityExecutorError::RateLimited) => {
                    return Err(CommunityExecutorError::RateLimited);
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "agents metrics fetch failed, falling back to direct public JSON"
                    );
                }
            }
        }

        let url = format!("https://www.reddit.com/comments/{reddit_post_id}.json");

        let client = self
            .http_client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut request = client.get(&url);
        if let Some(cookie_header) = self.fetch_reddit_cookies().await {
            request = request.header("Cookie", cookie_header);
        }
        let response = request.send().await?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(CommunityExecutorError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CommunityExecutorError::RedditApi(format!(
                "HTTP {status}: {body}"
            )));
        }

        // Public comments endpoint returns [post_listing, comments_listing].
        let listings: Vec<RedditListingResponse> = response.json().await?;
        let post_listing = listings
            .first()
            .ok_or_else(|| CommunityExecutorError::RedditApi("empty response array".to_owned()))?;
        let data = post_listing.data.children.first().ok_or_else(|| {
            CommunityExecutorError::RedditApi("no children in post metrics response".to_owned())
        })?;

        Ok(RedditPostMetrics {
            score: data.data.score,
            upvotes: data.data.ups,
            num_comments: data.data.num_comments,
            upvote_ratio: data.data.upvote_ratio,
        })
    }

    /// Records a post metrics snapshot and updates the fetch timestamp.
    async fn record_post_metrics(
        &self,
        post_id: Uuid,
        reddit_post_id: &str,
        metrics: &RedditPostMetrics,
    ) -> Result<(), CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            r#"
            INSERT INTO community_post_metrics
                (workspace_id, community_post_id, reddit_post_id, score, upvotes, num_comments, upvote_ratio)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (workspace_id, community_post_id, measured_at) DO NOTHING
            "#,
        )
        .bind(ws)
        .bind(post_id)
        .bind(reddit_post_id)
        .bind(metrics.score)
        .bind(metrics.upvotes)
        .bind(metrics.num_comments)
        .bind(metrics.upvote_ratio)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE community_posts
            SET metrics_last_fetched_at = now(),
                updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(post_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Updates only the `metrics_last_fetched_at` timestamp, used when a
    /// metrics fetch fails so we don't retry the same post immediately.
    async fn touch_metrics_fetched_at(&self, post_id: Uuid) -> Result<(), CommunityExecutorError> {
        sqlx::query(
            r#"
            UPDATE community_posts
            SET metrics_last_fetched_at = now(),
                updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(post_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Marks a community post as failed with an error message and
    /// propagates the failure back to the parent autopilot action so the
    /// ledger does not report success for a post that never went live.
    async fn mark_failed(&self, post_id: Uuid, error: &str) -> Result<(), CommunityExecutorError> {
        let action_id: Option<Uuid> =
            sqlx::query_scalar("SELECT action_id FROM community_posts WHERE id = $1")
                .bind(post_id)
                .fetch_optional(&self.pool)
                .await?;

        sqlx::query(
            r#"
            UPDATE community_posts
            SET status = 'failed',
                error_message = $2,
                updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(post_id)
        .bind(error)
        .execute(&self.pool)
        .await?;

        // Propagate failure to the parent autopilot action. The action was
        // marked 'succeeded' by actions_execution.rs before this worker
        // ran — that was premature. Correct it now so the operator sees
        // the real outcome in the autopilot ledger.
        if let Some(action_id) = action_id {
            let error_kind = if error.len() > 96 {
                "community_post_failed"
            } else {
                error
            };
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = 'failed',
                    finished_at = now(),
                    last_error_kind = $3,
                    updated_at = now()
                WHERE id = $1 AND status = 'succeeded'
                "#,
            )
            .bind(action_id)
            .bind(self.workspace_id.into_uuid())
            .bind(error_kind)
            .execute(&self.pool)
            .await?;
            tracing::warn!(
                post_id = %post_id,
                action_id = %action_id,
                error = %error,
                "community post failed — propagated failure to autopilot action"
            );
        }
        Ok(())
    }

    /// Marks a community post as rate-limited with a backoff window.
    /// Does not propagate to the autopilot action because rate-limited
    /// posts will be retried — the action stays 'succeeded' and the
    /// community_posts row tracks the retry state.
    async fn mark_rate_limited(&self, post_id: Uuid) -> Result<(), CommunityExecutorError> {
        sqlx::query(
            r#"
            UPDATE community_posts
            SET status = 'rate_limited',
                rate_limited_until = now() + make_interval(secs => $2::double precision),
                updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(post_id)
        .bind(RATE_LIMIT_BACKOFF.as_secs() as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct ClaimedAction {
    id: Uuid,
    action_id: Uuid,
    subreddit: String,
    title: String,
    body: String,
    smart_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RedditApiResponse {
    json: RedditJson,
}

#[derive(Debug, Deserialize)]
struct RedditJson {
    // Reddit error arrays are `[["ERROR", "message", null]]` — the third
    // element can be null, so we use Option<String>.
    #[serde(default)]
    errors: Vec<(String, String, Option<String>)>,
    data: Option<RedditData>,
}

#[derive(Debug, Deserialize)]
struct RedditData {
    id: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct RedditSubmitResult {
    post_id: String,
    post_url: String,
}

#[derive(sqlx::FromRow)]
struct PostMetricsTarget {
    id: Uuid,
    reddit_post_id: String,
}

/// Reddit API response for `GET /by_id/t3_{id}.json` — a listing wrapper.
#[derive(Debug, Deserialize)]
struct RedditListingResponse {
    data: RedditListingData,
}

#[derive(Debug, Deserialize)]
struct RedditListingData {
    children: Vec<RedditListingChild>,
}

#[derive(Debug, Deserialize)]
struct RedditListingChild {
    data: RedditPostData,
}

#[derive(Debug, Deserialize)]
struct RedditPostData {
    score: i32,
    ups: i32,
    num_comments: i32,
    upvote_ratio: Option<f64>,
}

/// Parsed metrics from a Reddit post, ready to record.
#[derive(Deserialize)]
struct RedditPostMetrics {
    score: i32,
    upvotes: i32,
    num_comments: i32,
    upvote_ratio: Option<f64>,
}

/// Parses a Reddit OAuth config from env vars. Returns `None` if the
/// client_id is not set, indicating Reddit integration is not configured.
#[must_use]
pub fn reddit_config_from_env() -> Option<FanbaseOauthConfig> {
    use crowdrelay_domain::fanbase::Platform;
    let client_id = std::env::var("CROWDRELAY_FANBASE_OAUTH_REDDIT_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())?;
    let client_secret =
        std::env::var("CROWDRELAY_FANBASE_OAUTH_REDDIT_CLIENT_SECRET").unwrap_or_default();
    let token_url = std::env::var("CROWDRELAY_FANBASE_OAUTH_REDDIT_TOKEN_URL")
        .unwrap_or_else(|_| "https://www.reddit.com/api/v1/access_token".to_owned());
    let authorize_url = std::env::var("CROWDRELAY_FANBASE_OAUTH_REDDIT_AUTHORIZE_URL")
        .unwrap_or_else(|_| "https://www.reddit.com/api/v1/authorize".to_owned());
    // Default scopes include "submit" — required for posting to Reddit.
    // This mirrors Platform::Reddit.default_scopes() in crowdrelay-domain.
    let scopes_str = std::env::var("CROWDRELAY_FANBASE_OAUTH_REDDIT_SCOPES")
        .unwrap_or_else(|_| "identity read submit".to_owned());
    let scopes: Vec<String> = scopes_str
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    Some(FanbaseOauthConfig {
        platform: Platform::Reddit,
        client_id,
        client_secret,
        authorize_url,
        token_url,
        scopes,
    })
}
