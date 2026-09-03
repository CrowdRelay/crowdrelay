//! Social post executor: tracks LLM-drafted `social-post` actions for
//! Instagram, Facebook, and X/Twitter.
//!
//! The autopilot marks `agent.content.request` actions as `succeeded` after
//! emitting the outbox event. This worker is the *internal executor* that
//! tracks the delivery — it polls for succeeded actions whose `template_id`
//! is `social-post` and that don't yet have a `social_posts` row, extracts
//! the platform and text from the draft, and records the result.
//!
//! ## Current mode: manual (default)
//! Instagram, Facebook, and X do not have simple Bot APIs like Telegram or
//! Discord. Posting requires either official OAuth APIs (Meta Graph API,
//! X API v2 — complex app review) or browser automation (Playwright driving
//! a logged-in session — not yet implemented in the agents service).
//!
//! Until auto-posting is wired, the executor runs in **manual mode**: it
//! creates `social_posts` rows and marks them `awaiting_manual_post`. The
//! operator sees the drafted posts in the control panel, posts them manually
//! to the platform, and registers the post URL via the API.
//!
//! This closes the dead-end: previously, social-post drafts were emitted to
//! the outbox but nothing tracked them. Now the brain sees reach events and
//! can measure the effect of social posts on fan growth.
//!
//! ## Anti-spam guardrails
//! - One post per platform per 12 hours (enforced via SQL check)
//! - Max 5 posts per 24 hours per workspace (enforced via SQL count)
//! - Rate-limited responses (HTTP 429) get `rate_limited` status with backoff
//!
//! ## Idempotency and crash recovery
//! `social_posts.action_id` is UNIQUE. The lifecycle is:
//!   `pending` → `posting` → `posted` (or `failed` / `rate_limited`)
//!
//! A crash during `posting` is recovered by reclaiming `posting` rows older
//! than 5 minutes — but we do NOT re-submit (to avoid duplicate posts). The
//! `social_posts` row is marked `failed`, and the parent autopilot action is
//! transitioned to `unknown` (NOT `failed`) because the post may have
//! actually succeeded — we lost confirmation, not the intervention.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

/// How often to poll for unprocessed social post actions.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// A `posting` row older than this is considered a crashed attempt.
const POSTING_STALE_THRESHOLD: Duration = Duration::from_secs(300);
/// Error-message prefix the stale-posting recovery stamps on crash-marked
/// rows. The receipt reconciliation sweep treats this prefix as "outcome
/// not establishable from CrowdRelay" and leaves the action `unknown`.
pub(crate) const CRASH_POSTING_ERROR_PREFIX: &str = "worker crashed during posting";
/// Rate limit backoff: how long to wait before retrying a rate-limited post.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(600);
/// Maximum posts per workspace per 24 hours.
const MAX_POSTS_PER_24H: i64 = 5;
/// Cooldown: no more than one post per platform per 12 hours.
const PLATFORM_COOLDOWN_HOURS: i32 = 12;
/// Watchdog for one executor cycle.
const CYCLE_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(120);
/// Maximum posts to claim in a single cycle.
const CLAIM_BATCH: i64 = 10;

#[derive(Debug, Error)]
pub enum SocialPostExecutorError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid platform: {0}")]
    InvalidPlatform(String),
    #[error("rate limited")]
    RateLimited,
}

#[derive(Clone)]
pub struct SocialPostExecutorWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    /// When true (default), posts are marked `awaiting_manual_post` — the
    /// operator posts manually and registers the post URL via the API.
    /// When false, the executor would post via platform APIs — but this is
    /// not yet implemented (requires Meta Graph API / X API integration).
    manual_mode: bool,
}

impl SocialPostExecutorWorker {
    /// Creates a new executor. In the current implementation, `manual_mode`
    /// is always `true` — auto-posting for Instagram/Facebook/X is not yet
    /// wired. The parameter exists so the wiring is ready when platform API
    /// support is added.
    #[must_use]
    pub fn new(pool: PgPool, workspace_id: WorkspaceId, manual_mode: bool) -> Self {
        Self {
            pool,
            workspace_id,
            poll_interval: POLL_INTERVAL,
            manual_mode,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    match timeout(CYCLE_WATCHDOG_TIMEOUT, self.run_once()).await {
                        Ok(Ok(processed)) if processed > 0 => {
                            tracing::info!(processed, "social post executor processed batch");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "social post executor cycle failed"),
                        Err(_) => tracing::warn!("social post executor cycle timed out"),
                    }
                }
            }
        }
    }

    async fn run_once(&self) -> Result<usize, SocialPostExecutorError> {
        self.recover_stale_posting().await?;
        let actions = self.claim_pending_actions().await?;
        let mut processed = 0;
        for action in &actions {
            match self.process_action(action).await {
                Ok(()) => processed += 1,
                Err(SocialPostExecutorError::RateLimited) => {
                    if let Err(e) = self.mark_rate_limited(action.id).await {
                        tracing::warn!(error = %e, "failed to mark rate_limited");
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        action_id = %action.action_id,
                        platform = %action.platform,
                        error = %error,
                        "failed to process social post"
                    );
                    let msg = error.to_string();
                    if let Err(e) = self.mark_failed(action.id, &msg).await {
                        tracing::warn!(error = %e, "failed to mark error");
                    }
                }
            }
        }
        Ok(processed)
    }

    /// Recovers `posting` rows that have been stuck longer than the stale
    /// threshold. The `social_posts` row is marked `failed`, and the parent
    /// autopilot action is transitioned to `unknown` — NOT `failed` — because
    /// the post may have actually succeeded.
    async fn recover_stale_posting(&self) -> Result<(), SocialPostExecutorError> {
        let ws = self.workspace_id.into_uuid();

        let stale_rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            r#"
            SELECT id, action_id FROM social_posts
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

        let post_ids: Vec<Uuid> = stale_rows.iter().map(|(id, _)| *id).collect();
        let result = sqlx::query(
            r#"
            UPDATE social_posts
            SET status = 'failed',
                error_message = $2,
                updated_at = now()
            WHERE id = ANY($1)
            "#,
        )
        .bind(&post_ids)
        .bind(format!(
            "{CRASH_POSTING_ERROR_PREFIX} — check platform manually"
        ))
        .execute(&self.pool)
        .await?;

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
            "recovered stale posting rows (social_posts=failed, action=unknown — check platform manually)"
        );
        Ok(())
    }

    /// Claims a batch of work in a single atomic transaction:
    /// 1. Inserts `pending` rows for succeeded `agent.content.request`
    ///    actions whose task template_id is `social-post` and whose draft
    ///    platform is instagram, facebook, or x (reddit is handled by
    ///    community_executor).
    /// 2. Reclaims existing `pending` rows and `rate_limited` rows past
    ///    their backoff.
    /// 3. Transitions claimed rows to `posting` using
    ///    `FOR UPDATE SKIP LOCKED`.
    async fn claim_pending_actions(&self) -> Result<Vec<ClaimedAction>, SocialPostExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let mut tx = self.pool.begin().await?;

        // Step 1: Insert pending rows for unprocessed succeeded actions.
        // The action payload has `kind: "request_agent_content"` and `draft`
        // containing the LLM output with `platform` and `text` fields. The
        // `template_id` is on the `agent_service_tasks` row referenced by
        // `payload->>'task_id'`. We join through it to filter on
        // `template_id = 'social-post'` and filter to non-reddit platforms
        // (reddit is handled by community_executor).
        // The existing `social_posts` table (migration 0168) requires a
        // `content` JSONB column — we store the full draft there.
        sqlx::query(
            r#"
            INSERT INTO social_posts (workspace_id, action_id, platform, content, smart_link, status)
            SELECT
                $1,
                a.id,
                a.payload->'draft'->>'platform',
                a.payload->'draft',
                a.payload->'draft'->>'cta_url',
                'pending'
            FROM viryaos_autopilot_actions a
            JOIN agent_service_tasks t ON t.id = (a.payload->>'task_id')::uuid
            WHERE a.workspace_id = $1
              AND a.action_kind = 'agent.content.request'
              AND a.status = 'succeeded'
              AND t.template_id = 'social-post'
              AND a.payload->'draft'->>'platform' IN ('instagram', 'facebook', 'x')
              AND NOT EXISTS (
                  SELECT 1 FROM social_posts sp WHERE sp.action_id = a.id
              )
            ON CONFLICT (action_id) DO NOTHING
            "#,
        )
        .bind(ws)
        .execute(&mut *tx)
        .await?;

        // Step 2: Claim pending and rate_limited (past backoff) rows.
        let rows = sqlx::query_as::<_, ClaimedAction>(
            r#"
            WITH claimed AS (
                UPDATE social_posts
                SET status = 'posting',
                    updated_at = now()
                WHERE id IN (
                    SELECT id FROM social_posts
                    WHERE workspace_id = $1
                      AND (
                          status = 'pending'
                          OR (status = 'rate_limited' AND rate_limited_until IS NOT NULL
                              AND rate_limited_until < now())
                      )
                    ORDER BY created_at
                    LIMIT $2
                    FOR UPDATE SKIP LOCKED
                )
                RETURNING id, action_id, platform
            )
            SELECT c.id, c.action_id, c.platform,
                   a.payload->'draft'->>'text' AS text,
                   a.payload->'draft'->>'cta_url' AS cta_url,
                   a.trace_id
            FROM claimed c
            LEFT JOIN viryaos_autopilot_actions a ON a.id = c.action_id
            "#,
        )
        .bind(ws)
        .bind(CLAIM_BATCH)
        .fetch_all(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(rows)
    }

    /// Processes a single claimed action: checks anti-spam guardrails,
    /// posts to the platform (or marks as awaiting manual post), and
    /// records the result.
    async fn process_action(&self, action: &ClaimedAction) -> Result<(), SocialPostExecutorError> {
        // Validate platform.
        if !matches!(action.platform.as_str(), "instagram" | "facebook" | "x") {
            return Err(SocialPostExecutorError::InvalidPlatform(
                action.platform.clone(),
            ));
        }

        // Anti-spam: check platform cooldown.
        if self.platform_on_cooldown(&action.platform).await? {
            tracing::info!(
                platform = %action.platform,
                "platform on 12h cooldown, skipping"
            );
            self.mark_failed(action.id, "platform on 12h cooldown")
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

        // Manual mode (default): mark as awaiting manual post.
        // The operator posts manually to the platform and registers the
        // post URL via the API.
        if self.manual_mode {
            sqlx::query(
                r#"
                UPDATE social_posts
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
                platform = %action.platform,
                "social post marked as awaiting manual post"
            );
            return Ok(());
        }

        // Auto mode: not yet implemented for Instagram/Facebook/X.
        // When Meta Graph API / X API integration is added, this is where
        // the API call would go. For now, fall back to manual mode.
        sqlx::query(
            r#"
            UPDATE social_posts
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
            platform = %action.platform,
            "auto-posting not yet implemented for this platform, marked as awaiting manual post"
        );
        Ok(())
    }

    /// Checks if this platform has been posted to within the cooldown window.
    async fn platform_on_cooldown(&self, platform: &str) -> Result<bool, SocialPostExecutorError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM social_posts
            WHERE workspace_id = $1
              AND platform = $2
              AND status = 'posted'
              AND posted_at > now() - make_interval(hours => $3::int)
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(platform)
        .bind(PLATFORM_COOLDOWN_HOURS)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// Checks if the workspace has reached the 24h post limit.
    async fn rate_limit_reached(&self) -> Result<bool, SocialPostExecutorError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM social_posts
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

    async fn mark_failed(
        &self,
        post_id: Uuid,
        reason: &str,
    ) -> Result<(), SocialPostExecutorError> {
        sqlx::query(
            r#"
            UPDATE social_posts
            SET status = 'failed',
                error_message = $2,
                updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(post_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_rate_limited(&self, post_id: Uuid) -> Result<(), SocialPostExecutorError> {
        sqlx::query(
            r#"
            UPDATE social_posts
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
    platform: String,
    #[allow(dead_code)]
    text: Option<String>,
    #[allow(dead_code)]
    cta_url: Option<String>,
    #[allow(dead_code)]
    trace_id: Option<Uuid>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_posting_error_prefix_is_stable() {
        assert!(!CRASH_POSTING_ERROR_PREFIX.is_empty());
        assert!(CRASH_POSTING_ERROR_PREFIX.contains("crashed"));
    }

    #[test]
    fn platform_cooldown_is_12_hours() {
        assert_eq!(PLATFORM_COOLDOWN_HOURS, 12);
    }

    #[test]
    fn max_posts_per_24h_is_bounded() {
        // Bounded between 1 and 10 — prevents both spam and total silence.
        const { assert!(MAX_POSTS_PER_24H > 0 && MAX_POSTS_PER_24H <= 10) };
    }
}
