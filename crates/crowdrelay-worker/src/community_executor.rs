//! Community engagement executor: posts approved community.engage.request
//! actions to Reddit via the agents service browser session.
//!
//! The autopilot marks `RequestCommunityEngagement` actions as `succeeded`
//! after emitting the outbox event (the outbox delivers to external webhook
//! endpoints). This worker is the *internal executor* that actually posts
//! to Reddit — it polls for succeeded actions that don't yet have a
//! `community_posts` row, submits the post through the agents service's
//! logged-in browser session, and records the result.
//!
//! ## Anti-spam guardrails
//! - One post per subreddit per 7 days (enforced via SQL check before posting)
//! - Max 3 posts per 24 hours per workspace (enforced via SQL count)
//! - If no agents service is configured, the post is marked `failed` (not retried)
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
//! The `community_posts` row is marked `failed`, but the parent autopilot
//! action is transitioned to `unknown` (NOT `failed`) because the Reddit post
//! may have actually succeeded — we lost confirmation, not the intervention.
//! The experiment assignment is also transitioned to `unknown`, which excludes
//! it from both realized-treatment and failed-treatment counts in the causal
//! learner. Unknown is non-terminal: it can later resolve to `executed` or
//! `failed` via reconciliation.
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
use crowdrelay_infra::reddit_proxy::read_reddit_proxy_from_db;
use serde::Deserialize;
use sqlx::PgPool;
use thiserror::Error;
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

/// A `posting` row older than this is considered a crashed attempt.
const POSTING_STALE_THRESHOLD: Duration = Duration::from_secs(300);

/// Error-message prefix the stale-posting recovery stamps on crash-marked
/// rows. The receipt reconciliation sweep treats this prefix as "outcome
/// not establishable from CrowdRelay" and leaves the action `unknown`.
pub(crate) const CRASH_POSTING_ERROR_PREFIX: &str = "worker crashed during posting";

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
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no agents service configured for Reddit posting")]
    NoAgentsService,
    #[error("rate limited by Reddit")]
    RateLimited,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct CommunityExecutorWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    http_client: Arc<RwLock<reqwest::Client>>,
    poll_interval: Duration,
    operation_timeout: Duration,
    public_origin: String,
    /// When true, the executor creates `community_posts` rows but does not
    /// post to Reddit. Posts are marked `awaiting_manual_post` — the operator
    /// posts manually and registers the URL via the API. Metrics polling
    /// still works via Reddit's public JSON endpoint.
    manual_mode: bool,
    /// Base URL of the agents service (for Reddit browser sessions).
    agent_service_url: String,
    /// Auth key for the agents service.
    agent_service_auth_key: Option<String>,
    /// Env-var proxy URL (manual override). The DB proxy from the sidecar
    /// takes precedence when available and fresh.
    env_proxy_url: Option<String>,
}

impl CommunityExecutorWorker {
    /// Creates a new executor. Returns an error if the HTTP client cannot be
    /// built (e.g. TLS backend failure). When `manual_mode` is false, the
    /// caller should ensure `agent_service_auth_key` is set for browser-based
    /// posting.
    ///
    /// # Errors
    /// Returns [`CommunityExecutorError::ClientBuild`] if the `reqwest` client
    /// cannot be initialized.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        operation_timeout: Duration,
        manual_mode: bool,
        proxy_url: Option<String>,
        agent_service_url: String,
        agent_service_auth_key: Option<String>,
    ) -> Result<Self, CommunityExecutorError> {
        let http_client = build_http_client(proxy_url.as_deref(), operation_timeout)?;
        let http_client = Arc::new(RwLock::new(http_client));
        let public_origin = std::env::var("CROWDRELAY_PUBLIC_ORIGIN")
            .unwrap_or_else(|_| DEFAULT_PUBLIC_ORIGIN.to_owned());
        Ok(Self {
            pool,
            workspace_id,
            http_client,
            poll_interval: POLL_INTERVAL,
            operation_timeout,
            public_origin,
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
                Err(CommunityExecutorError::NoAgentsService) => {
                    // No agents service — permanent failure, don't retry.
                    if let Err(e) = self
                        .mark_failed(action.id, "no agents service configured for Reddit posting")
                        .await
                    {
                        tracing::warn!(error = %e, "failed to mark no-agents-service");
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
    ///
    /// The community_posts row is marked `failed` (the DB record failed), but
    /// the parent autopilot action is transitioned to `unknown` — NOT `failed`.
    /// This is because the Reddit post may have actually succeeded; we simply
    /// lost confirmation. The action ledger maps `unknown` to UNKNOWN, which
    /// triggers reconciliation rather than treating it as a failed treatment.
    ///
    /// The experiment assignment is transitioned to `unknown` as well, so the
    /// causal learner excludes it from both realized-treatment and
    /// failed-treatment counts. Unknown is non-terminal: it can later resolve
    /// to `executed` or `failed` via reconciliation.
    async fn recover_stale_posting(&self) -> Result<(), CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();

        // Step 1: Find stale posting rows and collect their action_ids.
        let stale_rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            r#"
            SELECT id, action_id FROM community_posts
            WHERE workspace_id = $1
              AND status = 'posting'
              AND updated_at < now() - make_interval(secs => $2::double precision)
            "#,
        )
        .bind(ws)
        .bind(POSTING_STALE_THRESHOLD.as_secs() as i64)
        .fetch_all(&self.pool)
        .await?;

        if stale_rows.is_empty() {
            return Ok(());
        }

        // Step 2: Mark community_posts as failed (the DB record failed).
        let post_ids: Vec<Uuid> = stale_rows.iter().map(|(id, _)| *id).collect();
        let result = sqlx::query(
            r#"
            UPDATE community_posts
            SET status = 'failed',
                error_message = $2,
                updated_at = now()
            WHERE id = ANY($1)
            "#,
        )
        .bind(&post_ids)
        .bind(format!(
            "{CRASH_POSTING_ERROR_PREFIX} — check Reddit manually"
        ))
        .execute(&self.pool)
        .await?;

        // Step 3: Transition autopilot actions to 'unknown' (not 'failed').
        // The Reddit post may have succeeded — we lost confirmation, not
        // the intervention itself. Only transition actions that are currently
        // 'succeeded' or 'processing' (the premature success or in-flight state).
        let action_ids: Vec<Uuid> = stale_rows
            .iter()
            .filter_map(|(_, action_id)| *action_id)
            .collect();
        if !action_ids.is_empty() {
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = 'unknown',
                    finished_at = NULL,
                    updated_at = now()
                WHERE id = ANY($1)
                  AND workspace_id = $2
                  AND status IN ('succeeded', 'processing')
                "#,
            )
            .bind(&action_ids)
            .bind(ws)
            .execute(&self.pool)
            .await?;

            // Step 4: Transition experiment assignments to 'unknown'.
            // Unknown is excluded from both realized-treatment and
            // failed-treatment counts. It can later resolve to 'executed'
            // or 'failed' via reconciliation.
            sqlx::query(
                r#"
                UPDATE viryaos_experiment_assignments
                SET execution_status = 'unknown',
                    trace_id = COALESCE(trace_id, (SELECT trace_id FROM viryaos_autopilot_actions WHERE id = experiment_assignments.action_id))
                WHERE workspace_id = $1
                  AND action_id = ANY($2)
                  AND execution_status = 'dispatched'
                "#,
            )
            .bind(ws)
            .bind(&action_ids)
            .execute(&self.pool)
            .await?;
        }

        tracing::info!(
            recovered = result.rows_affected(),
            "recovered stale posting rows (community_posts=failed, action=unknown — check Reddit manually)"
        );
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
            WITH claimed AS (
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
            )
            SELECT c.id, c.action_id, c.subreddit, c.title, c.body, c.smart_link,
                   a.trace_id
            FROM claimed c
            LEFT JOIN viryaos_autopilot_actions a ON a.id = c.action_id
            "#,
        )
        .bind(ws)
        .fetch_all(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(rows)
    }

    /// Processes a single claimed action: checks anti-spam guardrails,
    /// posts to Reddit via the agents service browser, and records the result.
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

        // Browser-only: the agents service posts through a real logged-in
        // browser session — the only Reddit access path that works reliably.
        // No OAuth fallback: if the agents service is unavailable, the post
        // fails and the operator can re-approve.
        if self.agent_service_auth_key.is_none() {
            return Err(CommunityExecutorError::NoAgentsService);
        }

        // Build the post body, appending the smart link as a full URL if present.
        let post_body = self.build_post_body(&action.body, action.smart_link.as_deref());

        let reddit_result = self.submit_via_agent_browser(action, &post_body).await?;

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
        sqlx::query(r#"INSERT INTO viryaos_reach_events (workspace_id, action_id, recipient_kind, recipient_id, channel, template_id, estimated_reach, status, metadata, trace_id) VALUES ($1, $2, 'subreddit_audience', $3, 'reddit_post', 'community-engager', $5, 'delivered', jsonb_build_object('subreddit', $3, 'post_url', $4), $6) ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL DO NOTHING"#).bind(self.workspace_id.into_uuid()).bind(action.action_id).bind(&action.subreddit).bind(&reddit_result.post_url).bind(100_i32).bind(action.trace_id).execute(&self.pool).await?; // reach ledger — estimated_reach=100 as a conservative default for subreddit broadcasts (actual subscriber count not available at this layer)
        // Transition the experiment assignment execution_status from
        // dispatched → executed. This is the actual execution boundary:
        // the external intervention (Reddit post) has been confirmed.
        // Monotonic: only dispatched → executed is allowed; if the
        // assignment is not in 'dispatched' state, this is a no-op.
        // Propagate trace_id from the autopilot action for trace continuity.
        sqlx::query(
            r#"
            UPDATE viryaos_experiment_assignments
            SET execution_status = 'executed',
                trace_id = COALESCE(trace_id, (SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $2))
            WHERE workspace_id = $1
              AND action_id = $2
              AND execution_status = 'dispatched'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(action.action_id)
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
            .header(
                "X-Trace-Id",
                action.trace_id.map(|id| id.to_string()).unwrap_or_default(),
            )
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
            .map_err(|e| tracing::warn!(%e, "reddit cookies fetch failed"))
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| tracing::warn!(%e, "reddit cookies response was not json"))
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
            .ok_or_else(|| CommunityExecutorError::NoAgentsService)?;
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
                WHERE id = $1 AND workspace_id = $2 AND status = 'succeeded'
                "#,
            )
            .bind(action_id)
            .bind(self.workspace_id.into_uuid())
            .bind(error_kind)
            .execute(&self.pool)
            .await?;
            // Transition the experiment assignment execution_status from
            // dispatched → failed. The external intervention was attempted
            // but definitively failed. Monotonic: only dispatched → failed
            // is allowed; if the assignment is not in 'dispatched' state,
            // this is a no-op.
            sqlx::query(
                r#"
                UPDATE viryaos_experiment_assignments
                SET execution_status = 'failed',
                    trace_id = COALESCE(trace_id, (SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $2))
                WHERE workspace_id = $1
                  AND action_id = $2
                  AND execution_status = 'dispatched'
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(action_id)
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
    trace_id: Option<Uuid>,
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
