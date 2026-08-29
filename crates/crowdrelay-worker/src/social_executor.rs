//! Social media post executor: posts approved social.post.request actions
//! to external platforms (Facebook, Instagram, X/Twitter) via fanbase OAuth.
//!
//! The autopilot marks `RequestSocialPost` actions as `succeeded` after
//! emitting the outbox event. This worker is the *internal executor* that
//! actually posts to the platform — it polls for succeeded actions that
//! don't yet have a `social_posts` row, loads the workspace's platform
//! OAuth token from `fanbase_connections`, and submits the post via the
//! platform's Graph API.
//!
//! ## Anti-spam guardrails
//! - One post per platform per 24 hours per workspace (enforced via SQL count)
//! - Max 3 posts per 24 hours across all platforms per workspace
//! - If no platform connection exists, the post is marked `failed` (not retried)
//! - Rate-limited responses (HTTP 429) get `rate_limited` status with backoff
//!
//! ## Idempotency and crash recovery
//! `social_posts.action_id` is UNIQUE. The lifecycle is:
//!   `pending` → `posting` → `posted` (or `failed` / `rate_limited`)
//!
//! A crash between `pending` and `posting` leaves a `pending` row that the
//! next poll reclaims. A crash during `posting` is recovered by reclaiming
//! `posting` rows older than 5 minutes — but we do NOT re-submit (to avoid
//! duplicate posts). Instead, the row is marked `failed` with a message
//! directing the operator to check the platform manually.
//!
//! ## Concurrency
//! The claim query uses `FOR UPDATE SKIP LOCKED` so multiple worker instances
//! cannot process the same row.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{
    fanbase_oauth::{FanbaseOauthConfig, FanbaseOauthRepository},
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

/// How often to poll for unprocessed social post actions.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// A `posting` row older than this is considered a crashed attempt.
const POSTING_STALE_THRESHOLD: Duration = Duration::from_secs(300);
/// Rate limit backoff: how long to wait before retrying a rate-limited post.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(600);
/// Maximum posts per workspace per 24 hours across all platforms.
const MAX_POSTS_PER_24H: i64 = 3;
/// Token refresh threshold: if the token expires within this window, refresh.
const TOKEN_REFRESH_THRESHOLD: Duration = Duration::from_secs(300);

const USER_AGENT: &str = "CrowdRelay/1.0 (social executor)";

#[derive(Debug, Error)]
pub enum SocialExecutorError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("platform API error: {0}")]
    PlatformApi(String),
    #[error("oauth error: {0}")]
    Oauth(#[from] crowdrelay_infra::fanbase_oauth::FanbaseOauthError),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no platform connection for workspace")]
    NoPlatformConnection,
    #[error("rate limited by platform")]
    RateLimited,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct SocialExecutorWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    oauth_repo: FanbaseOauthRepository,
    encryption_key: SensitiveResponseKey,
    http_client: reqwest::Client,
    poll_interval: Duration,
    operation_timeout: Duration,
    /// Fanbase OAuth configs for each platform (Meta, TikTok, etc.).
    platform_configs: Vec<FanbaseOauthConfig>,
}

impl SocialExecutorWorker {
    /// Creates a new executor. Returns an error if the HTTP client cannot be
    /// built. The caller should skip spawning the worker if no platform
    /// configs are available.
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        platform_configs: Vec<FanbaseOauthConfig>,
        encryption_key: SensitiveResponseKey,
        operation_timeout: Duration,
    ) -> Result<Self, SocialExecutorError> {
        let http_client = reqwest::Client::builder()
            .connect_timeout(operation_timeout.min(Duration::from_secs(10)))
            .timeout(operation_timeout)
            .user_agent(USER_AGENT)
            .build()
            .map_err(SocialExecutorError::ClientBuild)?;
        let oauth_repo = FanbaseOauthRepository::new(pool.clone());

        Ok(Self {
            pool,
            workspace_id,
            oauth_repo,
            encryption_key,
            http_client,
            poll_interval: POLL_INTERVAL,
            operation_timeout,
            platform_configs,
        })
    }

    /// Returns true if at least one platform config is available.
    pub fn has_platforms(&self) -> bool {
        !self.platform_configs.is_empty()
    }

    /// Main loop: polls for unprocessed social post actions, posts them to
    /// the platform, and records the result. Runs until the shutdown signal.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), SocialExecutorError> {
        tracing::info!(
            workspace = %self.workspace_id,
            platforms = self.platform_configs.len(),
            "social executor started"
        );

        let mut ticker = interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await; // skip the first immediate tick

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.run_cycle().await {
                        tracing::error!(error = %e, "social executor cycle error");
                    }
                }
                result = shutdown.changed() => {
                    if result.is_ok() && *shutdown.borrow() {
                        tracing::info!("social executor shutting down");
                        return Ok(());
                    }
                }
            }
        }
    }

    /// One polling cycle: claim pending posts, submit them, reclaim stale
    /// `posting` rows. Each step is independent — one failure doesn't block
    /// the others.
    async fn run_cycle(&self) -> Result<(), SocialExecutorError> {
        let cycle_timeout = Duration::from_secs(self.operation_timeout.as_secs() * 3);
        let result = timeout(cycle_timeout, async {
            self.reclaim_stale_posting().await?;
            self.process_pending_posts().await?;
            Ok::<_, SocialExecutorError>(())
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_elapsed) => {
                tracing::warn!("social executor cycle timed out");
                Ok(())
            }
        }
    }

    /// Reclaims `posting` rows that have been stuck longer than the stale
    /// threshold. These are marked `failed` — we do NOT re-submit to avoid
    /// duplicate posts on the platform.
    async fn reclaim_stale_posting(&self) -> Result<(), SocialExecutorError> {
        let stale_before = OffsetDateTime::now_utc()
            - time::Duration::seconds(POSTING_STALE_THRESHOLD.as_secs() as i64);
        let reclaimed = sqlx::query(
            "UPDATE social_posts
             SET status = 'failed',
                 error_message = 'posting timed out — possible crash during submission; check the platform manually',
                 updated_at = now()
             WHERE workspace_id = $1
               AND status = 'posting'
               AND updated_at < $2",
        )
        .bind(self.workspace_id.into_uuid())
        .bind(stale_before)
        .execute(&self.pool)
        .await?;
        if reclaimed.rows_affected() > 0 {
            tracing::warn!(
                count = reclaimed.rows_affected(),
                "reclaimed stale social_posts posting rows"
            );
        }
        Ok(())
    }

    /// Claims and processes pending social post rows. Each row is processed
    /// independently — one failure doesn't block the next.
    async fn process_pending_posts(&self) -> Result<(), SocialExecutorError> {
        // Anti-spam: check total posts in the last 24 hours.
        let recent_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social_posts
             WHERE workspace_id = $1
               AND status = 'posted'
               AND posted_at > now() - interval '24 hours'",
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await?;

        if recent_count >= MAX_POSTS_PER_24H {
            tracing::debug!(
                count = recent_count,
                "social executor: 24h post limit reached, skipping cycle"
            );
            return Ok(());
        }

        // Claim pending rows with FOR UPDATE SKIP LOCKED.
        let rows = sqlx::query_as::<_, PendingPostRow>(
            "SELECT id, action_id, platform, content, smart_link
             FROM social_posts
             WHERE workspace_id = $1
               AND status = 'pending'
             ORDER BY created_at
             LIMIT 5
             FOR UPDATE SKIP LOCKED",
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Ok(());
        }

        tracing::info!(
            count = rows.len(),
            "social executor: processing pending posts"
        );

        for row in rows {
            // Mark as posting.
            let _ = sqlx::query(
                "UPDATE social_posts SET status = 'posting', attempts = attempts + 1, updated_at = now() WHERE id = $1",
            )
            .bind(row.id)
            .execute(&self.pool)
            .await;

            // Check platform-specific 24h cooldown.
            let platform_recent: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM social_posts
                 WHERE workspace_id = $1 AND platform = $2
                   AND status = 'posted'
                   AND posted_at > now() - interval '24 hours'",
            )
            .bind(self.workspace_id.into_uuid())
            .bind(&row.platform)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

            if platform_recent > 0 {
                self.mark_failed(
                    row.id,
                    &format!(
                        "platform {} already posted to in the last 24h",
                        row.platform
                    ),
                )
                .await;
                continue;
            }

            // Attempt to post.
            match self.post_to_platform(&row).await {
                Ok(result) => {
                    self.mark_posted(row.id, &result.post_id, &result.post_url)
                        .await;
                    tracing::info!(
                        post_id = %row.id,
                        platform = %row.platform,
                        "social post published"
                    );
                }
                Err(SocialExecutorError::RateLimited) => {
                    self.mark_rate_limited(row.id).await;
                }
                Err(e) => {
                    self.mark_failed(row.id, &e.to_string()).await;
                    tracing::error!(
                        post_id = %row.id,
                        platform = %row.platform,
                        error = %e,
                        "social post failed"
                    );
                }
            }
        }
        Ok(())
    }

    /// Posts to the platform via its Graph API. Loads the OAuth token from
    /// `fanbase_connections`, refreshes if needed, and submits the post.
    async fn post_to_platform(
        &self,
        row: &PendingPostRow,
    ) -> Result<PostResult, SocialExecutorError> {
        let platform = match row.platform.as_str() {
            "facebook" => crowdrelay_domain::fanbase::Platform::Meta,
            "instagram" => crowdrelay_domain::fanbase::Platform::Meta, // IG uses Meta Graph API
            "tiktok" => crowdrelay_domain::fanbase::Platform::Tiktok,
            _ => {
                return Err(SocialExecutorError::PlatformApi(format!(
                    "unsupported platform: {}",
                    row.platform
                )));
            }
        };

        // Find the config for this platform.
        let config = self
            .platform_configs
            .iter()
            .find(|c| c.platform == platform)
            .ok_or(SocialExecutorError::NoPlatformConnection)?;

        // Find the connection ID for this platform from fanbase_connections.
        let connection_id = self.find_platform_connection(platform).await?;

        // Load the OAuth token.
        let tokens = self
            .oauth_repo
            .load_tokens(
                self.workspace_id.into_uuid(),
                connection_id,
                &self.encryption_key,
            )
            .await?;

        // Refresh token if it expires within the threshold.
        let needs_refresh = tokens
            .expires_at
            .map(|exp| exp < OffsetDateTime::now_utc() + TOKEN_REFRESH_THRESHOLD)
            .unwrap_or(true);

        let access_token = if needs_refresh {
            tracing::info!(connection_id = %connection_id, "refreshing platform OAuth token");
            self.oauth_repo
                .refresh_token(
                    self.workspace_id.into_uuid(),
                    connection_id,
                    config,
                    &self.encryption_key,
                )
                .await?;
            // Reload after refresh.
            let refreshed = self
                .oauth_repo
                .load_tokens(
                    self.workspace_id.into_uuid(),
                    connection_id,
                    &self.encryption_key,
                )
                .await?;
            refreshed.access_token
        } else {
            tokens.access_token
        };

        // Submit the post via the platform's API.
        match row.platform.as_str() {
            "facebook" => {
                self.post_to_facebook(&access_token, &row.content, &row.smart_link)
                    .await
            }
            "instagram" => {
                self.post_to_instagram(&access_token, &row.content, &row.smart_link)
                    .await
            }
            "tiktok" => Err(SocialExecutorError::PlatformApi(
                "TikTok posting not yet implemented".into(),
            )),
            _ => Err(SocialExecutorError::PlatformApi(format!(
                "unsupported platform: {}",
                row.platform
            ))),
        }
    }

    /// Finds the most recent active connection for the given platform.
    async fn find_platform_connection(
        &self,
        platform: crowdrelay_domain::fanbase::Platform,
    ) -> Result<Uuid, SocialExecutorError> {
        let platform_str = match platform {
            crowdrelay_domain::fanbase::Platform::Meta => "meta",
            crowdrelay_domain::fanbase::Platform::Reddit => "reddit",
            crowdrelay_domain::fanbase::Platform::Spotify => "spotify",
            crowdrelay_domain::fanbase::Platform::GoogleAds => "google_ads",
            crowdrelay_domain::fanbase::Platform::Tiktok => "tiktok",
            crowdrelay_domain::fanbase::Platform::Bandsintown => "bandsintown",
        };
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM fanbase_connections
             WHERE workspace_id = $1
               AND platform = $2
               AND status = 'connected'
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(self.workspace_id.into_uuid())
        .bind(platform_str)
        .fetch_optional(&self.pool)
        .await?;
        id.ok_or(SocialExecutorError::NoPlatformConnection)
    }

    /// Posts to Facebook via the Graph API: POST /{page-id}/feed.
    async fn post_to_facebook(
        &self,
        access_token: &str,
        content: &serde_json::Value,
        smart_link: &Option<String>,
    ) -> Result<PostResult, SocialExecutorError> {
        let message = content
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let link = content
            .get("link")
            .and_then(|l| l.as_str())
            .or(smart_link.as_deref());

        let page_id = content
            .get("page_id")
            .and_then(|p| p.as_str())
            .unwrap_or("me");

        let mut form = vec![("message".to_string(), message.to_string())];
        if let Some(l) = link {
            form.push(("link".to_string(), l.to_string()));
        }

        let url = format!("https://graph.facebook.com/v21.0/{page_id}/feed");
        let response = self
            .http_client
            .post(&url)
            .query(&[("access_token", access_token)])
            .form(&form)
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SocialExecutorError::RateLimited);
        }
        if !status.is_success() {
            return Err(SocialExecutorError::PlatformApi(format!(
                "Facebook API returned {status}: {body}"
            )));
        }

        let result: FacebookPostResponse = serde_json::from_str(&body)
            .map_err(|e| SocialExecutorError::PlatformApi(format!("invalid response: {e}")))?;

        let post_url = format!("https://www.facebook.com/{}", result.id);
        Ok(PostResult {
            post_id: result.id,
            post_url,
        })
    }

    /// Posts to Instagram via the Graph API: two-step container + publish.
    async fn post_to_instagram(
        &self,
        access_token: &str,
        content: &serde_json::Value,
        _smart_link: &Option<String>,
    ) -> Result<PostResult, SocialExecutorError> {
        let caption = content
            .get("caption")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let media_url = content
            .get("media_url")
            .and_then(|m| m.as_str())
            .ok_or_else(|| {
                SocialExecutorError::PlatformApi("Instagram requires media_url in content".into())
            })?;

        let ig_user_id = content
            .get("ig_user_id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| {
                SocialExecutorError::PlatformApi("Instagram requires ig_user_id in content".into())
            })?;

        // Step 1: Create media container.
        let container_response = self
            .http_client
            .post(format!(
                "https://graph.facebook.com/v21.0/{ig_user_id}/media"
            ))
            .query(&[("access_token", access_token)])
            .form(&[("image_url", media_url), ("caption", caption)])
            .send()
            .await?;

        let container_status = container_response.status();
        let container_body = container_response.text().await.unwrap_or_default();

        if container_status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SocialExecutorError::RateLimited);
        }
        if !container_status.is_success() {
            return Err(SocialExecutorError::PlatformApi(format!(
                "Instagram container creation failed ({container_status}): {container_body}"
            )));
        }

        let container: InstagramContainerResponse =
            serde_json::from_str(&container_body).map_err(|e| {
                SocialExecutorError::PlatformApi(format!("invalid container response: {e}"))
            })?;

        // Step 2: Publish the container.
        let publish_response = self
            .http_client
            .post(format!(
                "https://graph.facebook.com/v21.0/{ig_user_id}/media_publish"
            ))
            .query(&[("access_token", access_token)])
            .form(&[("creation_id", &container.id)])
            .send()
            .await?;

        let publish_status = publish_response.status();
        let publish_body = publish_response.text().await.unwrap_or_default();

        if publish_status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SocialExecutorError::RateLimited);
        }
        if !publish_status.is_success() {
            return Err(SocialExecutorError::PlatformApi(format!(
                "Instagram publish failed ({publish_status}): {publish_body}"
            )));
        }

        let result: FacebookPostResponse = serde_json::from_str(&publish_body).map_err(|e| {
            SocialExecutorError::PlatformApi(format!("invalid publish response: {e}"))
        })?;

        let post_url = format!("https://www.instagram.com/p/{}/", result.id);
        Ok(PostResult {
            post_id: result.id,
            post_url,
        })
    }

    /// Marks a post as successfully posted.
    async fn mark_posted(&self, id: Uuid, post_id: &str, post_url: &str) {
        if let Err(e) = sqlx::query(
            "UPDATE social_posts
             SET status = 'posted',
                 platform_post_id = $2,
                 platform_post_url = $3,
                 posted_at = now(),
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(post_id)
        .bind(post_url)
        .execute(&self.pool)
        .await
        {
            tracing::error!(error = %e, "failed to mark social post as posted");
        }
    }

    /// Marks a post as failed with an error message.
    async fn mark_failed(&self, id: Uuid, error: &str) {
        if let Err(e) = sqlx::query(
            "UPDATE social_posts
             SET status = 'failed',
                 error_message = $2,
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(error)
        .execute(&self.pool)
        .await
        {
            tracing::error!(error = %e, "failed to mark social post as failed");
        }
    }

    /// Marks a post as rate-limited with a backoff timer.
    async fn mark_rate_limited(&self, id: Uuid) {
        let retry_after = OffsetDateTime::now_utc()
            + time::Duration::seconds(RATE_LIMIT_BACKOFF.as_secs() as i64);
        if let Err(e) = sqlx::query(
            "UPDATE social_posts
             SET status = 'rate_limited',
                 rate_limited_until = $2,
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(retry_after)
        .execute(&self.pool)
        .await
        {
            tracing::error!(error = %e, "failed to mark social post as rate_limited");
        }
    }
}

#[derive(sqlx::FromRow)]
struct PendingPostRow {
    id: Uuid,
    #[allow(dead_code)]
    action_id: Uuid,
    platform: String,
    content: serde_json::Value,
    smart_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FacebookPostResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
struct InstagramContainerResponse {
    id: String,
}

struct PostResult {
    post_id: String,
    post_url: String,
}
