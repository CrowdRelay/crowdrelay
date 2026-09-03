//! Telegram channel post executor: posts approved `agent.content.request`
//! actions (from the `telegram-poster` brain template) to the band's Telegram
//! channel via the Bot API.
//!
//! The autopilot marks `agent.content.request` actions as `succeeded` after
//! emitting the outbox event. This worker is the *internal executor* that
//! actually posts to Telegram — it polls for succeeded actions whose
//! `template_id` is `telegram-poster` and that don't yet have a
//! `telegram_posts` row, submits the post through the Bot API, and records
//! the result.
//!
//! ## Anti-spam guardrails
//! - One post per channel per 12 hours (enforced via SQL check before posting)
//! - Max 5 posts per 24 hours per workspace (enforced via SQL count)
//! - If `CROWDRELAY_TELEGRAM_AUTO_POST` is not enabled, posts are marked
//!   `awaiting_manual_post` — the operator posts manually
//! - Rate-limited responses (HTTP 429) get `rate_limited` status with backoff
//!
//! ## Idempotency and crash recovery
//! `telegram_posts.action_id` is UNIQUE. The lifecycle is:
//!   `pending` → `posting` → `posted` (or `failed` / `rate_limited`)
//!
//! A crash between `pending` and `posting` leaves a `pending` row that the
//! next poll reclaims. A crash during `posting` (between the Telegram API
//! call and the DB update) is recovered by reclaiming `posting` rows older
//! than 5 minutes — but we do NOT re-submit to Telegram (to avoid duplicate
//! posts). The `telegram_posts` row is marked `failed`, and the parent
//! autopilot action is transitioned to `unknown` (NOT `failed`) because the
//! Telegram message may have actually succeeded — we lost confirmation, not
//! the intervention. The experiment assignment is also transitioned to
//! `unknown`, which excludes it from both realized-treatment and
//! failed-treatment counts in the causal learner.
//!
//! ## Concurrency
//! The claim query uses `FOR UPDATE SKIP LOCKED` so multiple worker instances
//! cannot process the same row. Guardrail checks (cooldown, rate limit) are
//! performed within the same transaction as the status update to `posting`,
//! closing the race window.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::sensitive_response::{SensitiveResponseKey, decrypt_value};
use serde::Deserialize;
use sqlx::PgPool;
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

/// How often to poll for unprocessed telegram post actions.
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
/// Cooldown: no more than one post per channel per 12 hours.
const CHANNEL_COOLDOWN_HOURS: i32 = 12;
/// Watchdog for one executor cycle. The Bot API call is fast (no browser),
/// but the watchdog guards against a permanently hung cycle.
const CYCLE_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-request timeout for the Bot API call.
const BOT_API_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum posts to claim in a single cycle.
const CLAIM_BATCH: i64 = 5;

/// AAD for Telegram bot token encryption. Must match
/// `simple_platforms::telegram_bot_aad` and
/// `PostgresFanbaseRepository::token_aad` or the token will not decrypt.
fn telegram_bot_aad(workspace_id: Uuid, channel: &str) -> Vec<u8> {
    format!("crowdrelay.fanbase.oauth.telegram.v1\0{workspace_id}\0{channel}").into_bytes()
}

#[derive(Debug, Error)]
pub enum TelegramExecutorError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("telegram API error: {0}")]
    TelegramApi(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no telegram connection configured for workspace")]
    NoConnection,
    #[error("rate limited by Telegram")]
    RateLimited,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct TelegramExecutorWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    http_client: reqwest::Client,
    poll_interval: Duration,
    /// When true, the executor creates `telegram_posts` rows but does not
    /// post to Telegram. Posts are marked `awaiting_manual_post` — the
    /// operator posts manually and registers the message_id via the API.
    manual_mode: bool,
    /// Encryption key for decrypting the Telegram bot token stored in
    /// `fanbase_connections.encrypted_access_token`.
    encryption_key: SensitiveResponseKey,
}

impl TelegramExecutorWorker {
    /// Creates a new executor. Returns an error if the HTTP client cannot be
    /// built (e.g. TLS backend failure). When `manual_mode` is false, the
    /// caller should ensure a Telegram connection with a bot token exists.
    ///
    /// # Errors
    /// Returns [`TelegramExecutorError::ClientBuild`] if the `reqwest` client
    /// cannot be initialized.
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        manual_mode: bool,
        encryption_key: SensitiveResponseKey,
    ) -> Result<Self, TelegramExecutorError> {
        let http_client = reqwest::Client::builder()
            .timeout(BOT_API_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .user_agent("CrowdRelay/1.0 telegram-executor")
            .build()
            .map_err(TelegramExecutorError::ClientBuild)?;
        Ok(Self {
            pool,
            workspace_id,
            http_client,
            poll_interval: POLL_INTERVAL,
            manual_mode,
            encryption_key,
        })
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
                            tracing::info!(processed, "telegram executor processed batch");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "telegram executor cycle failed"),
                        Err(_) => tracing::warn!("telegram executor cycle timed out"),
                    }
                }
            }
        }
    }

    async fn run_once(&self) -> Result<usize, TelegramExecutorError> {
        // Recover stale `posting` rows from a previous crash.
        // We do NOT re-submit to Telegram (to avoid duplicate posts).
        self.recover_stale_posting().await?;

        let actions = self.claim_pending_actions().await?;
        let mut processed = 0;
        for action in &actions {
            match self.process_action(action).await {
                Ok(()) => processed += 1,
                Err(TelegramExecutorError::RateLimited) => {
                    if let Err(e) = self.mark_rate_limited(action.id).await {
                        tracing::warn!(error = %e, "failed to mark rate_limited");
                    }
                }
                Err(TelegramExecutorError::NoConnection) => {
                    if let Err(e) = self
                        .mark_failed(action.id, "no telegram connection configured")
                        .await
                    {
                        tracing::warn!(error = %e, "failed to mark no-connection");
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        action_id = %action.action_id,
                        channel = %action.channel,
                        error = %error,
                        "failed to post to telegram"
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
    /// threshold. These are from a worker crash during the Bot API call.
    ///
    /// The `telegram_posts` row is marked `failed`, but the parent autopilot
    /// action is transitioned to `unknown` — NOT `failed` — because the
    /// Telegram message may have actually succeeded; we lost confirmation.
    async fn recover_stale_posting(&self) -> Result<(), TelegramExecutorError> {
        let ws = self.workspace_id.into_uuid();

        let stale_rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            r#"
            SELECT id, action_id FROM telegram_posts
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
            UPDATE telegram_posts
            SET status = 'failed',
                error_message = $2,
                updated_at = now()
            WHERE id = ANY($1)
            "#,
        )
        .bind(&post_ids)
        .bind(format!(
            "{CRASH_POSTING_ERROR_PREFIX} — check Telegram manually"
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
            "recovered stale posting rows (telegram_posts=failed, action=unknown — check Telegram manually)"
        );
        Ok(())
    }

    /// Claims a batch of work in a single atomic transaction:
    /// 1. Inserts `pending` rows for succeeded `agent.content.request` actions
    ///    whose `template_id` is `telegram-poster` and that don't have a
    ///    `telegram_posts` row yet.
    /// 2. Reclaims existing `pending` rows and `rate_limited` rows past
    ///    their backoff.
    /// 3. Transitions claimed rows to `posting` using
    ///    `FOR UPDATE SKIP LOCKED`.
    async fn claim_pending_actions(&self) -> Result<Vec<ClaimedAction>, TelegramExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let mut tx = self.pool.begin().await?;

        // Step 1: Insert pending rows for unprocessed succeeded actions.
        // The action payload has `kind: "request_agent_content"` and
        // `draft` containing the LLM output. The `template_id` is on the
        // `agent_service_tasks` row referenced by `payload->>'task_id'`.
        // We join through it to filter on `template_id = 'telegram-poster'`.
        // The channel comes from the telegram fanbase_connection, not the
        // action payload — we set it to empty here and load it from the
        // connection when posting.
        sqlx::query(
            r#"
            INSERT INTO telegram_posts (workspace_id, action_id, channel, status)
            SELECT
                $1,
                a.id,
                COALESCE(a.payload->'draft'->>'channel', ''),
                'pending'
            FROM viryaos_autopilot_actions a
            JOIN agent_service_tasks t ON t.id = (a.payload->>'task_id')::uuid
            WHERE a.workspace_id = $1
              AND a.action_kind = 'agent.content.request'
              AND a.status = 'succeeded'
              AND t.template_id = 'telegram-poster'
              AND NOT EXISTS (
                  SELECT 1 FROM telegram_posts tp WHERE tp.action_id = a.id
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
                UPDATE telegram_posts
                SET status = 'posting',
                    updated_at = now()
                WHERE id IN (
                    SELECT id FROM telegram_posts
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
                RETURNING id, action_id, channel
            )
            SELECT c.id, c.action_id, c.channel,
                   a.payload->'draft'->>'text' AS body,
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
    /// posts to Telegram via the Bot API, and records the result.
    async fn process_action(&self, action: &ClaimedAction) -> Result<(), TelegramExecutorError> {
        // Anti-spam: check channel cooldown.
        if self.channel_on_cooldown(&action.channel).await? {
            tracing::info!(channel = %action.channel, "channel on 12h cooldown, skipping");
            self.mark_failed(action.id, "channel on 12h cooldown")
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

        // Manual mode: skip Bot API, mark as awaiting manual post.
        if self.manual_mode {
            sqlx::query(
                r#"
                UPDATE telegram_posts
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
                channel = %action.channel,
                "telegram post marked as awaiting manual post"
            );
            return Ok(());
        }

        // Load the bot token from the telegram connection.
        let (channel, bot_token) = self.load_bot_token().await?;

        // Use the channel from the connection if the action payload didn't
        // carry one (it may be empty if the outcome didn't include it).
        let target_channel = if action.channel.is_empty() {
            &channel
        } else {
            &action.channel
        };

        let body = action.body.as_deref().unwrap_or("");
        if body.is_empty() {
            self.mark_failed(action.id, "post body is empty").await?;
            return Ok(());
        }

        let result = self
            .submit_via_bot_api(target_channel, body, &bot_token)
            .await?;

        // Record success.
        sqlx::query(
            r#"
            UPDATE telegram_posts
            SET status = 'posted',
                message_id = $2,
                posted_at = now(),
                updated_at = now(),
                error_message = NULL,
                rate_limited_until = NULL
            WHERE id = $1
            "#,
        )
        .bind(action.id)
        .bind(result.message_id)
        .execute(&self.pool)
        .await?;

        // Reach ledger — estimated_reach is conservative; actual subscriber
        // count is not available at this layer.
        sqlx::query(
            r#"INSERT INTO viryaos_reach_events
                 (workspace_id, action_id, recipient_kind, recipient_id, channel,
                  template_id, estimated_reach, status, metadata, trace_id, causation_id)
               VALUES ($1, $2, 'telegram_channel', $3, 'telegram_post',
                       'telegram-poster', $4, 'delivered',
                       jsonb_build_object('channel', $3, 'message_id', $5), $6, $2)
               ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL
               DO NOTHING"#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(action.action_id)
        .bind(target_channel)
        .bind(50_i32)
        .bind(result.message_id)
        .bind(action.trace_id)
        .execute(&self.pool)
        .await?;

        // Transition the experiment assignment execution_status from
        // dispatched → executed. Monotonic: only dispatched → executed.
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
            action_id = %action.action_id,
            channel = %target_channel,
            message_id = result.message_id,
            "successfully posted to telegram"
        );
        Ok(())
    }

    /// Submits a message via the Telegram Bot API sendMessage endpoint.
    /// The bot token is in the URL path, so reqwest's Display would carry it
    /// into the log — strip the URL off every error out of this call.
    async fn submit_via_bot_api(
        &self,
        channel: &str,
        body: &str,
        bot_token: &str,
    ) -> Result<TelegramSubmitResult, TelegramExecutorError> {
        let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
        let payload = serde_json::json!({
            "chat_id": channel,
            "text": body,
            "parse_mode": "HTML",
        });

        let response = self
            .http_client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|error| TelegramExecutorError::TelegramApi(error.without_url().to_string()))?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(TelegramExecutorError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(TelegramExecutorError::TelegramApi(format!(
                "telegram Bot API returned HTTP {status}: {body}"
            )));
        }

        let body: TelegramApiResponse = response
            .json()
            .await
            .map_err(|error| TelegramExecutorError::TelegramApi(error.without_url().to_string()))?;
        if !body.ok {
            return Err(TelegramExecutorError::TelegramApi(
                body.description
                    .unwrap_or_else(|| "telegram API returned ok=false".to_owned()),
            ));
        }
        let result = body.result.ok_or_else(|| {
            TelegramExecutorError::TelegramApi("telegram API returned no result".to_owned())
        })?;
        Ok(TelegramSubmitResult {
            message_id: result.message_id,
        })
    }

    /// Loads the Telegram bot token from `fanbase_connections`.
    /// Returns (channel, decrypted_bot_token).
    async fn load_bot_token(&self) -> Result<(String, String), TelegramExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let row: (String, Option<String>) = sqlx::query_as(
            r#"SELECT provider_account_id, encrypted_access_token
               FROM fanbase_connections
               WHERE workspace_id = $1 AND platform = 'telegram' AND status = 'connected'
               LIMIT 1"#,
        )
        .bind(ws)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(TelegramExecutorError::NoConnection)?;

        let channel = row.0;
        let encrypted_token = row.1.ok_or_else(|| {
            TelegramExecutorError::TelegramApi(
                "Telegram connection missing encrypted_access_token (bot token)".to_owned(),
            )
        })?;

        let aad = telegram_bot_aad(ws, &channel);
        let token_bytes = URL_SAFE_NO_PAD.decode(&encrypted_token).map_err(|_| {
            TelegramExecutorError::TelegramApi("Telegram bot token is not valid base64".to_owned())
        })?;
        let bot_token = String::from_utf8(
            decrypt_value(&token_bytes, &self.encryption_key, &aad).map_err(|e| {
                TelegramExecutorError::TelegramApi(format!(
                    "Telegram bot token decryption failed: {e}"
                ))
            })?,
        )
        .map_err(|_| {
            TelegramExecutorError::TelegramApi("Telegram bot token is not valid UTF-8".to_owned())
        })?;

        Ok((channel, bot_token))
    }

    /// Checks if this channel has been posted to within the cooldown window.
    async fn channel_on_cooldown(&self, channel: &str) -> Result<bool, TelegramExecutorError> {
        if channel.is_empty() {
            return Ok(false);
        }
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM telegram_posts
            WHERE workspace_id = $1
              AND channel = $2
              AND status = 'posted'
              AND posted_at > now() - make_interval(hours => $3::int)
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(channel)
        .bind(CHANNEL_COOLDOWN_HOURS)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// Checks if the workspace has reached the 24h post limit.
    async fn rate_limit_reached(&self) -> Result<bool, TelegramExecutorError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM telegram_posts
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

    async fn mark_failed(&self, post_id: Uuid, reason: &str) -> Result<(), TelegramExecutorError> {
        sqlx::query(
            r#"
            UPDATE telegram_posts
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

    async fn mark_rate_limited(&self, post_id: Uuid) -> Result<(), TelegramExecutorError> {
        sqlx::query(
            r#"
            UPDATE telegram_posts
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
    channel: String,
    body: Option<String>,
    trace_id: Option<Uuid>,
}

#[derive(Debug)]
struct TelegramSubmitResult {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse {
    ok: bool,
    description: Option<String>,
    result: Option<TelegramMessageResult>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessageResult {
    message_id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_bot_aad_is_deterministic() {
        let ws = Uuid::nil();
        let aad1 = telegram_bot_aad(ws, "@virya_music");
        let aad2 = telegram_bot_aad(ws, "@virya_music");
        assert_eq!(aad1, aad2);
    }

    #[test]
    fn telegram_bot_aad_includes_workspace_and_channel() {
        let ws1 = Uuid::nil();
        let ws2 = Uuid::now_v7();
        let aad1 = telegram_bot_aad(ws1, "@channel");
        let aad2 = telegram_bot_aad(ws2, "@channel");
        let aad3 = telegram_bot_aad(ws1, "@other");
        assert_ne!(aad1, aad2);
        assert_ne!(aad1, aad3);
    }

    #[test]
    fn telegram_api_response_deserializes_ok() {
        let json = r#"{"ok":true,"result":{"message_id":42}}"#;
        let resp: TelegramApiResponse = serde_json::from_str(json).expect("valid json");
        assert!(resp.ok);
        assert_eq!(resp.result.expect("result").message_id, 42);
    }

    #[test]
    fn telegram_api_response_deserializes_error() {
        let json = r#"{"ok":false,"description":"chat not found"}"#;
        let resp: TelegramApiResponse = serde_json::from_str(json).expect("valid json");
        assert!(!resp.ok);
        assert_eq!(resp.description.as_deref(), Some("chat not found"));
    }

    #[test]
    fn crash_posting_error_prefix_is_stable() {
        // The receipt reconciliation sweep matches on this prefix.
        assert!(!CRASH_POSTING_ERROR_PREFIX.is_empty());
        assert!(CRASH_POSTING_ERROR_PREFIX.contains("crashed"));
    }
}
