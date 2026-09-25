//! Social post executor: tracks LLM-drafted `social-post` actions for
//! Instagram, Facebook, and X/Twitter.
//!
//! The autopilot marks `agent.content.request` actions as `succeeded` after
//! emitting the outbox event. This worker is the *internal executor* that
//! tracks the delivery — it polls for succeeded actions whose `template_id`
//! is `social-post` and that don't yet have a `social_posts` row, extracts
//! the platform and text from the draft, and records the result.
//!
//! ## Modes, per platform
//!
//! `CROWDRELAY_SOCIAL_AUTO_POST` off (the default) drafts everything: the
//! executor creates `social_posts` rows, marks them `awaiting_manual_post`,
//! and the operator publishes and registers the URL.
//!
//! The live value is read from `tenant_settings.social_auto_post` on each
//! poll cycle (60s TTL cache), so an operator can flip it from the control
//! plane without a restart. The env var is the fallback when the database
//! is unreachable — a DB blip never silently enables publishing.
//!
//! With it on, the platforms diverge, and they diverge for reasons rather
//! than for want of work on one line:
//!
//! - **Facebook Pages publish.** A Business Manager system user token with
//!   `pages_manage_posts` on a Page the business owns posts to the Page feed
//!   through the Graph API. That is what system users are for; publishing to
//!   an owned asset is not the case app review governs. The token either
//!   carries the grant or the Graph API refuses the call, and a refusal holds
//!   the post with the platform's own message rather than retrying it.
//! - **Instagram publishes.** Two calls rather than one: a media container
//!   built from an image URL Meta fetches itself, then the publish. There is
//!   no text-only post on Instagram, so the image is the post — and the
//!   system chooses it, never the model. The selector reads the tenant's own
//!   active photo assets, least recently published first, so a run of posts
//!   rotates instead of repeating one picture.
//! - **X drafts.** Its write API is behind a paid tier this tenant does not
//!   hold.
//!
//! Every automatic post goes through `domain::publish_guard` first — the read
//! a person was doing before autonomy. A held post lands in the same operator
//! queue it was in before, carrying the reason.
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
use crowdrelay_domain::publish_guard::content_hash;
use sqlx::PgPool;
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

pub(crate) mod join_ask;
pub(crate) mod platforms;
pub(crate) mod reach;
pub(crate) mod tracked_links;

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

/// The reach filed when no metric sync has measured the audience yet — the
/// same conservative constant the telegram and discord paths use, because a
/// guessed denominator is worse than an honest small one.
const UNMEASURED_AUDIENCE_REACH: i32 = 10;

/// Graph API request timeout. A Page post is a small write; anything slower
/// than this is the API being unavailable, not the post being large.
const GRAPH_API_TIMEOUT: Duration = Duration::from_secs(30);

/// Pinned Graph API version, matching the one `growth_metric_sync` reads Page
/// metrics with. Meta deprecates versions on a schedule, so the version is a
/// thing to review rather than a default to inherit.
const GRAPH_API_VERSION: &str = "v21.0";

#[derive(Debug, Error)]
pub enum SocialPostExecutorError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid platform: {0}")]
    InvalidPlatform(String),
    #[error("rate limited")]
    RateLimited,
    #[error("HTTP client could not be built: {0}")]
    ClientBuild(reqwest::Error),
    #[error("graph api request failed: {0}")]
    GraphRequest(reqwest::Error),
    /// The Graph API refused the call. Carries the platform's own message,
    /// because "posting failed" is not something an operator can act on and
    /// "(#200) Requires pages_manage_posts permission" is.
    #[error("graph api refused the post: {0}")]
    GraphRefused(String),
    #[error("telegram bot api request failed: {0}")]
    TelegramRequest(reqwest::Error),
    /// The Bot API refused the call — same contract as `GraphRefused`:
    /// carries Telegram's own description ("chat not found", "bot is not
    /// an administrator") because that is what the operator can act on.
    #[error("telegram bot api refused the post: {0}")]
    TelegramRefused(String),
}

#[derive(Clone)]
pub struct SocialPostExecutorWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    /// When true (default), posts are marked `awaiting_manual_post` — the
    /// operator posts manually and registers the post URL via the API.
    ///
    /// When false, Facebook Pages and Instagram publish through the Graph
    /// API. X stays manual regardless: its write API is behind a paid tier
    /// this tenant does not hold. Saying that here rather than silently
    /// falling through is the difference between "not built" and "built and
    /// quietly doing nothing".
    ///
    /// This is the env-var fallback used at construction time. The live value
    /// is read from `tenant_settings.social_auto_post` on each poll cycle, so
    /// an operator can flip it from the control plane without a restart.
    env_manual_mode: bool,
    /// Page access token for the Graph API, when one is configured.
    ///
    /// The same credential `growth_metric_sync` already reads Page metrics
    /// with — a Business Manager system user token on assets the business
    /// owns. Publishing to an owned Page or Instagram account is what a system
    /// user is for; the token either carries the grant (`pages_manage_posts`
    /// for the Page, `instagram_content_publish` for the account) or the Graph
    /// API refuses the call, and a refusal holds the post rather than retrying
    /// it.
    facebook_page_access_token: Option<String>,
    http_client: reqwest::Client,
    /// The tenant's own public origin. A link in an automatically published
    /// post may point here and nowhere else — see `publish_guard`.
    public_origin: String,
    /// Opens the encrypted bot token on the telegram `fanbase_connections`
    /// row — the same key `TelegramExecutorWorker` uses, because it is the
    /// same credential.
    response_encryption_key: crowdrelay_infra::sensitive_response::SensitiveResponseKey,
    /// `CROWDRELAY_TELEGRAM_AUTO_POST` at construction: the worker-side kill
    /// switch for the telegram arm, separate from the tenant's
    /// `social_auto_post`. Off means telegram posts draft for a person.
    telegram_auto_post: bool,
}

impl SocialPostExecutorWorker {
    /// Creates a new executor.
    ///
    /// `manual_mode` false enables Facebook Page and Instagram publishing,
    /// and only when `facebook_page_access_token` is present — one Meta
    /// credential covers both. Everything else drafts and waits for an
    /// operator.
    ///
    /// # Errors
    /// Returns [`SocialPostExecutorError::ClientBuild`] if the HTTP client
    /// cannot be initialized.
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        manual_mode: bool,
        facebook_page_access_token: Option<String>,
        public_origin: String,
        response_encryption_key: crowdrelay_infra::sensitive_response::SensitiveResponseKey,
        telegram_auto_post: bool,
    ) -> Result<Self, SocialPostExecutorError> {
        let http_client = reqwest::Client::builder()
            .timeout(GRAPH_API_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .user_agent("CrowdRelay/1.0 social-post-executor")
            .build()
            .map_err(SocialPostExecutorError::ClientBuild)?;
        Ok(Self {
            pool,
            workspace_id,
            poll_interval: POLL_INTERVAL,
            env_manual_mode: manual_mode,
            facebook_page_access_token,
            http_client,
            public_origin,
            response_encryption_key,
            telegram_auto_post,
        })
    }

    /// Builds the worker or explains why it did not start. The mode log line
    /// lives here so `main` stays a wiring table: an operator reading the
    /// worker's source sees the mode its process claims, not a log statement
    /// a callsite could forget or contradict.
    pub fn build(
        pool: PgPool,
        workspace_id: WorkspaceId,
        manual_mode: bool,
        facebook_page_access_token: Option<String>,
        public_origin: String,
        response_encryption_key: crowdrelay_infra::sensitive_response::SensitiveResponseKey,
        telegram_auto_post: bool,
    ) -> Option<Self> {
        match Self::new(
            pool,
            workspace_id,
            manual_mode,
            facebook_page_access_token.clone(),
            public_origin,
            response_encryption_key,
            telegram_auto_post,
        ) {
            Ok(worker) => {
                if manual_mode {
                    tracing::info!(
                        "social post executor running in MANUAL MODE — posts are drafted and wait for an operator to publish them manually"
                    );
                } else {
                    tracing::info!(
                        has_facebook_token = facebook_page_access_token.is_some(),
                        "social post executor running in AUTOMATIC MODE — Facebook Pages publish; Instagram and X are drafted for an operator"
                    );
                }
                Some(worker)
            }
            Err(error) => {
                tracing::warn!(error = %error, "social post executor disabled: HTTP client build failed");
                None
            }
        }
    }

    /// Returns true when the executor should draft (manual mode) rather than
    /// publish automatically. Two switches gate publishing: the deployment
    /// kill switch (`CROWDRELAY_SOCIAL_AUTO_POST`, off forces manual no matter
    /// what the tenant setting says) and the tenant's own `social_auto_post`,
    /// read live from the database (60s TTL cache) so the control plane toggle
    /// takes effect without a restart. A settings read failure also holds —
    /// a database blip must never silently enable publishing.
    async fn is_manual_mode(&self) -> bool {
        if self.env_manual_mode {
            return true;
        }
        use crowdrelay_infra::tenant_settings::TenantSettingsRepository;
        let repo = TenantSettingsRepository::new(self.pool.clone());
        match repo.brand_settings(self.workspace_id.into_uuid()).await {
            Ok(settings) => !settings.social_auto_post,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to read social_auto_post from tenant settings; holding posts for a human"
                );
                true
            }
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

    /// Runs one drafting/publishing pass.
    ///
    /// Public so an integration test can drive it, matching
    /// `AgentOutcomeWorker::run_once`. The link it covers — a succeeded
    /// `agent.content.request` becoming a post artifact — had no test and no
    /// production row in any of the three post tables, so nothing anywhere
    /// showed whether it worked.
    ///
    /// In manual mode this only drafts: it writes the artifact row and makes
    /// no network call.
    pub async fn run_once(&self) -> Result<usize, SocialPostExecutorError> {
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
                UPDATE autopilot_actions
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
                UPDATE experiment_assignments AS ea
                SET execution_status = 'unknown',
                    trace_id = COALESCE(ea.trace_id, (SELECT trace_id FROM autopilot_actions WHERE id = ea.action_id))
                WHERE ea.workspace_id = $1
                  AND ea.action_id = ANY($2)
                  AND ea.execution_status = 'dispatched'
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
        // `smart_link_id` binds at insert when the draft already names a
        // tracked link (`/l/{slug}` or `{origin}/l/{slug}`) — the slug
        // namespace is ours, so the join cannot point anywhere else. A bare
        // destination URL resolves in the claim pass below, which mints the
        // `smart_links` row first.
        // Under a savepoint: without an agent service the joined table does
        // not exist, and there is nothing to materialise — see
        // `crate::foreign_relation`. The join-ask and the claim below still run.
        let mut materialize = sqlx::Acquire::begin(&mut *tx).await?;
        let materialized = sqlx::query(
            r#"
            INSERT INTO social_posts (workspace_id, action_id, platform, content, smart_link, smart_link_id, status)
            SELECT
                $1,
                a.id,
                a.payload->'draft'->>'platform',
                a.payload->'draft',
                COALESCE('/l/' || link.slug, a.payload->'draft'->>'cta_url'),
                link.id,
                'pending'
            FROM autopilot_actions a
            -- The regex guard precedes the cast: one action whose payload
            -- carries a non-uuid task_id would otherwise abort the whole
            -- claim statement every sweep and stall the executor.
            JOIN agent_service_tasks t
              ON a.payload->>'task_id' ~ '^[0-9a-fA-F-]{36}$'
             AND t.id = (a.payload->>'task_id')::uuid
            LEFT JOIN smart_links link
              ON link.workspace_id = a.workspace_id
             AND link.slug = substring(a.payload->'draft'->>'cta_url' from '/l/([a-zA-Z0-9][a-zA-Z0-9_-]*)$')
             -- Only our own link namespace counts: a bare `/l/` path or the
             -- tenant origin. A foreign host's `/l/` path would bind this
             -- post to one of our links it never meant.
             AND (a.payload->'draft'->>'cta_url' LIKE '/l/%'
                  OR a.payload->'draft'->>'cta_url' LIKE $2 || '/l/%')
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
        .bind(self.public_origin.trim_end_matches('/'))
        .execute(&mut *materialize)
        .await;
        match materialized {
            Ok(_) => materialize.commit().await?,
            Err(error) if crate::foreign_relation::is_undefined_table(&error) => {
                materialize.rollback().await?;
            }
            Err(error) => return Err(error.into()),
        }

        // Step 1b: the weekly join-ask (§5) — same claim shape, no task join.
        self.file_join_ask_posts(&mut tx).await?;

        // Step 2: Claim pending and rate_limited (past backoff) rows.
        let mut rows = sqlx::query_as::<_, ClaimedAction>(
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
                RETURNING id, action_id, platform, smart_link, smart_link_id, image_url
            )
            -- Two payload shapes reach this read: the agent draft nests its
            -- fields under `draft`, the join-ask carries them flat. COALESCE
            -- reads whichever exists — a draft payload has no top-level
            -- `text`, a join-ask has no `draft`.
            SELECT c.id, c.action_id, c.platform,
                   COALESCE(a.payload->'draft'->>'text', a.payload->>'text') AS text,
                   COALESCE(a.payload->'draft'->>'cta_url', a.payload->>'cta_url') AS cta_url,
                   c.smart_link, c.smart_link_id, c.image_url,
                   a.trace_id
            FROM claimed c
            LEFT JOIN autopilot_actions a ON a.id = c.action_id
            "#,
        )
        .bind(ws)
        .bind(CLAIM_BATCH)
        .fetch_all(&mut *tx)
        .await?;

        // Step 3: bind the tracked link for rows whose draft named a bare
        // destination. `cta_url` is where the model wanted the audience to
        // land; `smart_link` is the `/l/` redirect that lets the click be
        // counted. A destination the validator refuses stays untracked —
        // the post still goes out, and its click measurement abandons as
        // `no_tracked_link` rather than recording a zero that was never
        // observable.
        for row in &mut rows {
            if row.smart_link_id.is_some() {
                continue;
            }
            self.resolve_tracked_link(&mut tx, row).await?;
        }

        tx.commit().await?;
        Ok(rows)
    }

    /// Processes a single claimed action: checks anti-spam guardrails,
    /// posts to the platform (or marks as awaiting manual post), and
    /// records the result.
    async fn process_action(&self, action: &ClaimedAction) -> Result<(), SocialPostExecutorError> {
        if !matches!(
            action.platform.as_str(),
            "instagram" | "facebook" | "x" | "telegram"
        ) {
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
            self.mark_rate_limited(action.id).await?;
            return Ok(());
        }

        // Anti-spam: check 24h rate limit.
        if self.rate_limit_reached().await? {
            tracing::info!("24h post limit reached, skipping");
            self.mark_rate_limited(action.id).await?;
            return Ok(());
        }

        // Manual mode (default): mark as awaiting manual post.
        // The operator posts manually to the platform and registers the
        // post URL via the API.
        //
        // The live value is read from tenant_settings on each cycle so an
        // operator can flip it from the control plane without a restart.
        if self.is_manual_mode().await {
            sqlx::query(
                r#"
                UPDATE social_posts
                SET status = 'awaiting_manual_post',
                    updated_at = now()
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(self.workspace_id.into_uuid())
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

        // Automatic mode. Facebook Pages and Instagram publish through the
        // Graph API; Telegram through the Bot API when its own kill switch
        // is on; X still drafts, because its write API is behind a paid tier
        // this tenant does not hold. Held with the reason so the queue says
        // why rather than looking like a stuck job.
        if action.platform == "facebook" {
            return self.publish_to_facebook_page(action).await;
        }
        if action.platform == "instagram" {
            return self.publish_to_instagram(action).await;
        }
        if action.platform == "telegram" {
            return self.publish_to_telegram(action).await;
        }

        let reason = "x publishing needs a paid API tier";
        self.hold_for_human(action.id, reason).await?;
        tracing::info!(
            action_id = %action.action_id,
            platform = %action.platform,
            reason,
            "social post held for an operator: this platform does not publish automatically"
        );
        Ok(())
    }

    /// Parks a drafted post for an operator, with the reason it was held.
    ///
    /// `awaiting_manual_post` rather than `failed`: nothing went wrong with
    /// the delivery, and a person can still publish this.
    async fn hold_for_human(
        &self,
        post_id: Uuid,
        reason: &str,
    ) -> Result<(), SocialPostExecutorError> {
        sqlx::query(
            r#"
            UPDATE social_posts
            SET status = 'awaiting_manual_post',
                error_message = $3,
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(post_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Content hashes of what this platform published recently.
    async fn recent_content_hashes(
        &self,
        platform: &str,
    ) -> Result<std::collections::BTreeSet<String>, SocialPostExecutorError> {
        let bodies: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT content->>'text'
            FROM social_posts
            WHERE workspace_id = $1
              AND platform = $2
              AND status = 'posted'
              AND posted_at > now() - INTERVAL '30 days'
              AND content->>'text' IS NOT NULL
              -- A join-ask is a deliberate, cadence-gated rotation — not
              -- the accidental repeat this dedupe exists to catch. Counted
              -- here it would hold every re-used variant forever.
              AND NOT (content ? 'join_ask')
            ORDER BY posted_at DESC
            LIMIT 50
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(platform)
        .fetch_all(&self.pool)
        .await?;
        Ok(bodies.iter().map(|body| content_hash(body)).collect())
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
                error_message = $3,
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(self.workspace_id.into_uuid())
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
                rate_limited_until = now() + make_interval(secs => $3::double precision),
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(self.workspace_id.into_uuid())
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
    text: Option<String>,
    cta_url: Option<String>,
    /// The post's tracked link (`/l/{slug}`), once the draft's CTA has been
    /// bound to a `smart_links` row. `None` means the post goes out
    /// unmeasured — either it named no link or the destination failed
    /// validation.
    smart_link: Option<String>,
    smart_link_id: Option<Uuid>,
    /// The image the action carries — only join-ask rows set it (the
    /// tenant's `join_ask_image_url`). `None` means the platform's own
    /// fallback: Instagram rotates press assets, the others post without
    /// a photo.
    image_url: Option<String>,
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
