//! Discord channel post executor: posts approved `agent.content.request`
//! actions (from the `discord-poster` brain template) to the band's Discord
//! channel via the Bot API.
//!
//! The autopilot marks `agent.content.request` actions as `succeeded` after
//! emitting the outbox event. This worker is the *internal executor* that
//! actually posts to Discord — it polls for succeeded actions whose
//! `template_id` is `discord-poster` and that don't yet have a
//! `discord_posts` row, submits the post through the Bot API, and records
//! the result.
//!
//! ## Anti-spam guardrails
//! - One post per channel per 12 hours (enforced via SQL check before posting)
//! - Max 5 posts per 24 hours per workspace (enforced via SQL count)
//! - If `CROWDRELAY_DISCORD_AUTO_POST` is not enabled, posts are marked
//!   `awaiting_manual_post` — the operator posts manually
//! - Rate-limited responses (HTTP 429) get `rate_limited` status with backoff
//!
//! ## Idempotency and crash recovery
//! `discord_posts.action_id` is UNIQUE. The lifecycle is:
//!   `pending` → `posting` → `posted` (or `failed` / `rate_limited`)
//!
//! A crash during `posting` is recovered by reclaiming `posting` rows older
//! than 5 minutes — but we do NOT re-submit to Discord (to avoid duplicate
//! posts). The `discord_posts` row is marked `failed`, and the parent
//! autopilot action is transitioned to `unknown` (NOT `failed`) because the
//! Discord message may have actually succeeded — we lost confirmation, not
//! the intervention.

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

/// How often to poll for unprocessed discord post actions.
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
/// Watchdog for one executor cycle.
const CYCLE_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-request timeout for the Bot API call.
const BOT_API_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum posts to claim in a single cycle.
const CLAIM_BATCH: i64 = 5;

/// AAD for Discord bot token encryption. Must match the AAD used in
/// `PostgresFanbaseRepository::encrypt_token` when the connection was
/// created, or the token will not decrypt.
/// Whether a string is a Discord snowflake — the only shape a channel id
/// takes.
///
/// Discord snowflakes are decimal timestamps: 17 to 20 digits today, and
/// nothing else. Checking the shape is what turns "posted into the void" into
/// a refusal with a reason, and it is the specific guard against putting an
/// invite code where a channel belongs, which is the mistake this executor
/// shipped with.
fn is_discord_snowflake(value: &str) -> bool {
    let value = value.trim();
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

fn discord_bot_aad(workspace_id: Uuid, channel_id: &str) -> Vec<u8> {
    format!("crowdrelay.fanbase.oauth.discord.v1\0{workspace_id}\0{channel_id}").into_bytes()
}

#[derive(Debug, Error)]
pub enum DiscordExecutorError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("discord API error: {0}")]
    DiscordApi(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no discord connection configured for workspace")]
    NoConnection,
    #[error("rate limited by Discord")]
    RateLimited,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Clone)]
pub struct DiscordExecutorWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    http_client: reqwest::Client,
    poll_interval: Duration,
    /// When true, the executor creates `discord_posts` rows but does not
    /// post to Discord. Posts are marked `awaiting_manual_post`.
    manual_mode: bool,
    /// Encryption key for decrypting the Discord bot token stored in
    /// `fanbase_connections.encrypted_access_token`.
    encryption_key: SensitiveResponseKey,
}

impl DiscordExecutorWorker {
    /// Creates a new executor. Returns an error if the HTTP client cannot be
    /// built (e.g. TLS backend failure).
    ///
    /// # Errors
    /// Returns [`DiscordExecutorError::ClientBuild`] if the `reqwest` client
    /// cannot be initialized.
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        manual_mode: bool,
        encryption_key: SensitiveResponseKey,
    ) -> Result<Self, DiscordExecutorError> {
        let http_client = reqwest::Client::builder()
            .timeout(BOT_API_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .user_agent("CrowdRelay/1.0 discord-executor")
            .build()
            .map_err(DiscordExecutorError::ClientBuild)?;
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
                            tracing::info!(processed, "discord executor processed batch");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "discord executor cycle failed"),
                        Err(_) => tracing::warn!("discord executor cycle timed out"),
                    }
                }
            }
        }
    }

    async fn run_once(&self) -> Result<usize, DiscordExecutorError> {
        self.recover_stale_posting().await?;
        let actions = self.claim_pending_actions().await?;
        let mut processed = 0;
        for action in &actions {
            match self.process_action(action).await {
                Ok(()) => processed += 1,
                Err(DiscordExecutorError::RateLimited) => {
                    if let Err(e) = self.mark_rate_limited(action.id).await {
                        tracing::warn!(error = %e, "failed to mark rate_limited");
                    }
                }
                Err(DiscordExecutorError::NoConnection) => {
                    if let Err(e) = self
                        .mark_failed(action.id, "no discord connection configured")
                        .await
                    {
                        tracing::warn!(error = %e, "failed to mark no-connection");
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        action_id = %action.action_id,
                        channel_id = %action.channel_id,
                        error = %error,
                        "failed to post to discord"
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
    /// threshold. The `discord_posts` row is marked `failed`, and the parent
    /// autopilot action is transitioned to `unknown` — NOT `failed` — because
    /// the Discord message may have actually succeeded.
    async fn recover_stale_posting(&self) -> Result<(), DiscordExecutorError> {
        let ws = self.workspace_id.into_uuid();

        let stale_rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            r#"
            SELECT id, action_id FROM discord_posts
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
            UPDATE discord_posts
            SET status = 'failed',
                error_message = $2,
                updated_at = now()
            WHERE id = ANY($1)
            "#,
        )
        .bind(&post_ids)
        .bind(format!(
            "{CRASH_POSTING_ERROR_PREFIX} — check Discord manually"
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
            "recovered stale posting rows (discord_posts=failed, action=unknown — check Discord manually)"
        );
        Ok(())
    }

    /// Claims a batch of work in a single atomic transaction:
    /// 1. Inserts `pending` rows for succeeded `agent.content.request` actions
    ///    whose `template_id` is `discord-poster`.
    /// 2. Reclaims existing `pending` rows and `rate_limited` rows past
    ///    their backoff.
    /// 3. Transitions claimed rows to `posting` using
    ///    `FOR UPDATE SKIP LOCKED`.
    async fn claim_pending_actions(&self) -> Result<Vec<ClaimedAction>, DiscordExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let mut tx = self.pool.begin().await?;

        // Step 1: Insert pending rows for unprocessed succeeded actions.
        // The action payload has `kind: "request_agent_content"` and `draft`
        // containing the LLM output. The `template_id` is not in the action
        // payload — it's on the `agent_service_tasks` row referenced by
        // `payload->>'task_id'`. We join through it to filter on
        // `template_id = 'discord-poster'`.
        sqlx::query(
            r#"
            INSERT INTO discord_posts (workspace_id, action_id, channel_id, status)
            SELECT
                $1,
                a.id,
                COALESCE(a.payload->>'channel_id', ''),
                'pending'
            FROM viryaos_autopilot_actions a
            JOIN agent_service_tasks t ON t.id = (a.payload->>'task_id')::uuid
            WHERE a.workspace_id = $1
              AND a.action_kind = 'agent.content.request'
              AND a.status = 'succeeded'
              AND t.template_id = 'discord-poster'
              AND NOT EXISTS (
                  SELECT 1 FROM discord_posts dp WHERE dp.action_id = a.id
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
                UPDATE discord_posts
                SET status = 'posting',
                    updated_at = now()
                WHERE id IN (
                    SELECT id FROM discord_posts
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
                RETURNING id, action_id, channel_id
            )
            SELECT c.id, c.action_id, c.channel_id,
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
    /// posts to Discord via the Bot API, and records the result.
    async fn process_action(&self, action: &ClaimedAction) -> Result<(), DiscordExecutorError> {
        // The cooldown is about the channel actually posted in, so the
        // connection is resolved before anything is checked against it.
        //
        // This used to check `action.channel_id`, which comes from the draft
        // and is almost always empty — and `channel_on_cooldown` returns
        // false for an empty string, so the twelve-hour limit never applied
        // to anything. The same empty value was written into
        // `discord_posts.channel_id`, so even a correct check would have had
        // nothing to count. Two halves of one hole, both invisible until the
        // first post.
        //
        // In manual mode there is no connection to resolve and no post to
        // rate-limit; the draft is written and an operator decides.
        let posting_target = if self.manual_mode {
            None
        } else {
            Some(self.load_bot_token().await?)
        };
        if let Some((ref channel_id, _)) = posting_target
            && self.channel_on_cooldown(channel_id).await?
        {
            tracing::info!(channel_id = %channel_id, "channel on 12h cooldown, skipping");
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
                UPDATE discord_posts
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
                channel_id = %action.channel_id,
                "discord post marked as awaiting manual post"
            );
            return Ok(());
        }

        // Resolved above, before the cooldown check that depends on it.
        // Manual mode returned above, so automatic mode always resolved one.
        // Saying so with an error rather than a panic keeps a future edit to
        // the mode logic from taking the worker down.
        let Some((channel_id, bot_token)) = posting_target else {
            return Err(DiscordExecutorError::NoConnection);
        };

        // The connection's channel is the only posting target. The action
        // payload also carries a `channel_id`, written by the drafting model,
        // and it used to win whenever it was non-empty — so a model could
        // name any channel on any server the bot can see and the bot would
        // post there. An operator configures where the bot may speak; a
        // draft does not get to redirect it.
        //
        // A payload channel that disagrees is worth saying out loud once: it
        // means the prompt is producing a field nothing should act on.
        if !action.channel_id.is_empty() && action.channel_id != channel_id {
            tracing::warn!(
                action_id = %action.action_id,
                drafted_channel = %action.channel_id,
                configured_channel = %channel_id,
                "ignoring the channel the draft named; posting to the configured channel"
            );
        }
        let target_channel = &channel_id;

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
            UPDATE discord_posts
            SET status = 'posted',
                message_id = $2,
                -- The row was inserted with the draft's channel, which is
                -- normally empty. Recording the channel actually posted in is
                -- what makes the twelve-hour cooldown able to count anything.
                channel_id = $3,
                posted_at = now(),
                updated_at = now(),
                error_message = NULL,
                rate_limited_until = NULL
            WHERE id = $1
            "#,
        )
        .bind(action.id)
        .bind(&result.message_id)
        .bind(target_channel)
        .execute(&self.pool)
        .await?;

        // Reach ledger.
        sqlx::query(
            r#"INSERT INTO viryaos_reach_events
                 (workspace_id, action_id, recipient_kind, recipient_id, channel,
                  template_id, estimated_reach, status, metadata, trace_id, causation_id)
               VALUES ($1, $2, 'discord_channel', $3, 'discord_post',
                       'discord-poster', $4, 'delivered',
                       jsonb_build_object('channel_id', $3, 'message_id', $5), $6, $2)
               ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL
               DO NOTHING"#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(action.action_id)
        .bind(target_channel)
        .bind(50_i32)
        .bind(&result.message_id)
        .bind(action.trace_id)
        .execute(&self.pool)
        .await?;

        // Transition the experiment assignment execution_status.
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
            channel_id = %target_channel,
            message_id = %result.message_id,
            "successfully posted to discord"
        );
        Ok(())
    }

    /// Submits a message via the Discord Bot API.
    /// `POST /channels/{channel_id}/messages` with `Authorization: Bot {token}`.
    async fn submit_via_bot_api(
        &self,
        channel_id: &str,
        body: &str,
        bot_token: &str,
    ) -> Result<DiscordSubmitResult, DiscordExecutorError> {
        let url = format!("https://discord.com/api/v10/channels/{channel_id}/messages");
        let payload = serde_json::json!({
            "content": body,
        });

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bot {bot_token}"))
            .json(&payload)
            .send()
            .await
            .map_err(|error| {
                // Strip the URL from the error — it contains the channel ID
                // but not the token (token is in the header).
                DiscordExecutorError::DiscordApi(error.to_string())
            })?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(DiscordExecutorError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(DiscordExecutorError::DiscordApi(format!(
                "discord Bot API returned HTTP {status}: {body}"
            )));
        }

        let result: DiscordMessageResponse = response
            .json()
            .await
            .map_err(|error| DiscordExecutorError::DiscordApi(error.to_string()))?;
        Ok(DiscordSubmitResult {
            message_id: result.id,
        })
    }

    /// Loads the Discord bot token and channel ID from `fanbase_connections`.
    /// Returns (channel_id, decrypted_bot_token).
    ///
    /// The channel is `provider_account_id`; the invite code the member-count
    /// sync reads lives in `external_account_ref`.
    ///
    /// One column used to hold both, and the two readers disagreed about
    /// which: metrics read it as an invite code, this function read it as a
    /// channel. Production held `BBdDV6gVy`, so a post would have gone to
    /// `POST /channels/BBdDV6gVy/messages`. It never failed out loud because
    /// no discord post had ever been attempted. Splitting the two meanings
    /// across the two columns is what fixed it; the shape check below is what
    /// keeps them from being swapped back.
    async fn load_bot_token(&self) -> Result<(String, String), DiscordExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let row: (Option<String>, Option<String>) = sqlx::query_as(
            r#"SELECT provider_account_id, encrypted_access_token
               FROM fanbase_connections
               WHERE workspace_id = $1 AND platform = 'discord' AND status = 'connected'
                 AND encrypted_access_token IS NOT NULL
               LIMIT 1"#,
        )
        .bind(ws)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(DiscordExecutorError::NoConnection)?;

        let channel_id = row.0.ok_or_else(|| {
            DiscordExecutorError::DiscordApi(
                "Discord connection has no provider_account_id — set it to the numeric \
                 channel id the bot should post in (Developer Mode, right-click the \
                 channel, Copy Channel ID). The invite code the member-count sync reads \
                 belongs in external_account_ref."
                    .to_owned(),
            )
        })?;
        if !is_discord_snowflake(&channel_id) {
            return Err(DiscordExecutorError::DiscordApi(format!(
                "Discord provider_account_id {channel_id:?} is not a channel id. A channel \
                 id is 17 to 20 digits; this looks like an invite code or a name, which \
                 means the channel and the invite code have been swapped."
            )));
        }
        let encrypted_token = row.1.ok_or_else(|| {
            DiscordExecutorError::DiscordApi(
                "Discord connection missing encrypted_access_token (bot token)".to_owned(),
            )
        })?;

        let aad = discord_bot_aad(ws, &channel_id);
        let token_bytes = URL_SAFE_NO_PAD.decode(&encrypted_token).map_err(|_| {
            DiscordExecutorError::DiscordApi("Discord bot token is not valid base64".to_owned())
        })?;
        let bot_token = String::from_utf8(
            decrypt_value(&token_bytes, &self.encryption_key, &aad).map_err(|e| {
                DiscordExecutorError::DiscordApi(format!(
                    "Discord bot token decryption failed: {e}"
                ))
            })?,
        )
        .map_err(|_| {
            DiscordExecutorError::DiscordApi("Discord bot token is not valid UTF-8".to_owned())
        })?;

        Ok((channel_id, bot_token))
    }

    /// Checks if this channel has been posted to within the cooldown window.
    async fn channel_on_cooldown(&self, channel_id: &str) -> Result<bool, DiscordExecutorError> {
        if channel_id.is_empty() {
            return Ok(false);
        }
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM discord_posts
            WHERE workspace_id = $1
              AND channel_id = $2
              AND status = 'posted'
              AND posted_at > now() - make_interval(hours => $3::int)
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(channel_id)
        .bind(CHANNEL_COOLDOWN_HOURS)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// Checks if the workspace has reached the 24h post limit.
    async fn rate_limit_reached(&self) -> Result<bool, DiscordExecutorError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM discord_posts
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

    async fn mark_failed(&self, post_id: Uuid, reason: &str) -> Result<(), DiscordExecutorError> {
        sqlx::query(
            r#"
            UPDATE discord_posts
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

    async fn mark_rate_limited(&self, post_id: Uuid) -> Result<(), DiscordExecutorError> {
        sqlx::query(
            r#"
            UPDATE discord_posts
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
    channel_id: String,
    body: Option<String>,
    trace_id: Option<Uuid>,
}

#[derive(Debug)]
struct DiscordSubmitResult {
    message_id: String,
}

#[derive(Debug, Deserialize)]
struct DiscordMessageResponse {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_id_is_a_snowflake_and_an_invite_code_is_not() {
        // Production holds `provider_account_id = 'BBdDV6gVy'`, the invite
        // code the member-count sync reads. It was also what this executor
        // would have posted to. A snowflake is 17-20 digits; nothing else is
        // a channel.
        assert!(is_discord_snowflake("1234567890123456789"));
        assert!(is_discord_snowflake("12345678901234567"));
        assert!(is_discord_snowflake("12345678901234567890"));
        assert!(is_discord_snowflake("  1234567890123456789  "));

        assert!(!is_discord_snowflake("BBdDV6gVy"), "an invite code");
        assert!(!is_discord_snowflake("general"), "a channel name");
        assert!(!is_discord_snowflake(""), "nothing at all");
        assert!(
            !is_discord_snowflake("1234567890123456"),
            "16 digits, too short"
        );
        assert!(
            !is_discord_snowflake("123456789012345678901"),
            "21 digits, too long"
        );
        assert!(
            !is_discord_snowflake("12345678901234567x"),
            "digits with a stray character"
        );
        assert!(
            !is_discord_snowflake("#1234567890123456789"),
            "a channel mention, not an id"
        );
    }

    #[test]
    fn discord_bot_aad_is_deterministic() {
        let ws = Uuid::nil();
        let aad1 = discord_bot_aad(ws, "123456789");
        let aad2 = discord_bot_aad(ws, "123456789");
        assert_eq!(aad1, aad2);
    }

    #[test]
    fn discord_bot_aad_includes_workspace_and_channel() {
        let ws1 = Uuid::nil();
        let ws2 = Uuid::now_v7();
        let aad1 = discord_bot_aad(ws1, "123");
        let aad2 = discord_bot_aad(ws2, "123");
        let aad3 = discord_bot_aad(ws1, "456");
        assert_ne!(aad1, aad2);
        assert_ne!(aad1, aad3);
    }

    #[test]
    fn discord_message_response_deserializes() {
        let json = r#"{"id": "1234567890123456789", "channel_id": "987654321"}"#;
        let resp: DiscordMessageResponse = serde_json::from_str(json).expect("valid json");
        assert_eq!(resp.id, "1234567890123456789");
    }

    #[test]
    fn crash_posting_error_prefix_is_stable() {
        assert!(!CRASH_POSTING_ERROR_PREFIX.is_empty());
        assert!(CRASH_POSTING_ERROR_PREFIX.contains("crashed"));
    }
}
