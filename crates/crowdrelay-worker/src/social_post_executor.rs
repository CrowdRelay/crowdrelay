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
use crowdrelay_domain::publish_guard::{
    PublishChannel, PublishContext, content_hash, review_outbound_post,
};
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
        })
    }

    /// Returns true when the executor should draft (manual mode) rather than
    /// publish automatically. Reads the live tenant setting from the database
    /// (60s TTL cache); falls back to the env-var value on any error so a
    /// database blip never silently enables publishing.
    async fn is_manual_mode(&self) -> bool {
        use crowdrelay_infra::tenant_settings::TenantSettingsRepository;
        let repo = TenantSettingsRepository::new(self.pool.clone());
        match repo.brand_settings(self.workspace_id.into_uuid()).await {
            Ok(settings) => !settings.social_auto_post,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to read social_auto_post from tenant settings; falling back to env default"
                );
                self.env_manual_mode
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
                UPDATE viryaos_experiment_assignments AS ea
                SET execution_status = 'unknown',
                    trace_id = COALESCE(ea.trace_id, (SELECT trace_id FROM viryaos_autopilot_actions WHERE id = ea.action_id))
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

        // Automatic mode. Facebook Pages and Instagram publish; X still
        // drafts, because its write API is behind a paid tier this tenant does
        // not hold. Held with the reason so the queue says why rather than
        // looking like a stuck job.
        if action.platform == "facebook" {
            return self.publish_to_facebook_page(action).await;
        }
        if action.platform == "instagram" {
            return self.publish_to_instagram(action).await;
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

    /// Publishes a drafted caption to the tenant's own Instagram account.
    ///
    /// Two calls, because Instagram publishing is two steps: build a media
    /// container from an image URL Meta fetches itself, then publish the
    /// container. There is no text-only post on Instagram, so an image is not
    /// decoration here — it is the post.
    ///
    /// **The system chooses the image, never the model.** A model naming an
    /// image URL is the same risk as a model naming a link: it can point
    /// anywhere, and what publishes under the band's name would be whatever it
    /// picked. The selector reads the tenant's own asset rows and nothing
    /// else, so an image that is not already CrowdRelay's cannot be published.
    async fn publish_to_instagram(
        &self,
        action: &ClaimedAction,
    ) -> Result<(), SocialPostExecutorError> {
        let Some(token) = self.facebook_page_access_token.as_ref() else {
            self.hold_for_human(action.id, "no meta access token is configured")
                .await?;
            return Ok(());
        };
        let Some(account_id) = self.instagram_account_id().await? else {
            self.hold_for_human(
                action.id,
                "no connected instagram professional account to post to",
            )
            .await?;
            return Ok(());
        };
        let caption = action.text.as_deref().unwrap_or("").trim();
        if caption.is_empty() {
            self.hold_for_human(action.id, "the draft has no caption")
                .await?;
            return Ok(());
        }
        let Some(image_url) = self.next_instagram_image().await? else {
            // Not a failure and not a defect: the tenant has published no
            // photo the system may use. An operator can fix it by adding one,
            // which is why the reason says what is missing.
            self.hold_for_human(
                action.id,
                "no image available: add an active photo press asset to post on instagram",
            )
            .await?;
            return Ok(());
        };

        let recent = self.recent_content_hashes("instagram").await?;
        let verdict = review_outbound_post(
            caption,
            &PublishContext {
                channel: PublishChannel::Instagram,
                approved_origins: &[self.public_origin.as_str()],
                recent_content_hashes: &recent,
            },
        );
        if let Some(reason) = verdict.hold_reason() {
            tracing::info!(
                action_id = %action.action_id,
                reason = reason.as_str(),
                "instagram post held for an operator by the publish guard"
            );
            self.hold_for_human(action.id, reason.as_str()).await?;
            return Ok(());
        }

        match self
            .submit_to_instagram(&account_id, caption, &image_url, token)
            .await
        {
            Ok(media_id) => {
                // Post + re-anchor in one commit: the measurement window
                // starts when the audience could see the post, not at
                // dispatch — a deferred draft must not be observed across
                // dead pre-exposure time.
                let mut posted_tx = self.pool.begin().await?;
                sqlx::query(
                    r#"
                    UPDATE social_posts
                    SET status = 'posted',
                        platform_post_id = $3,
                        image_url = $4,
                        posted_at = now(),
                        updated_at = now(),
                        error_message = NULL
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(self.workspace_id.into_uuid())
                .bind(action.id)
                .bind(&media_id)
                .bind(&image_url)
                .execute(&mut *posted_tx)
                .await?;
                crowdrelay_infra::fanbase::anchor_content_measurements_to_publication(
                    &mut posted_tx,
                    self.workspace_id.into_uuid(),
                    "social_posts",
                    action.id,
                )
                .await?;
                posted_tx.commit().await?;
                tracing::info!(
                    action_id = %action.action_id,
                    media_id = %media_id,
                    "instagram post published"
                );
                Ok(())
            }
            Err(SocialPostExecutorError::GraphRefused(message)) => {
                tracing::warn!(
                    action_id = %action.action_id,
                    error = %message,
                    "instagram refused the post; holding it for an operator"
                );
                self.hold_for_human(action.id, &format!("instagram refused the post: {message}"))
                    .await?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Creates the media container and publishes it. Returns the media id.
    ///
    /// A container that is created and never published is an orphan Meta
    /// cleans up on its own, so a failure between the two steps costs nothing
    /// and must not be retried into a duplicate post.
    async fn submit_to_instagram(
        &self,
        account_id: &str,
        caption: &str,
        image_url: &str,
        token: &str,
    ) -> Result<String, SocialPostExecutorError> {
        let creation_id = self
            .graph_post(
                &format!("https://graph.facebook.com/{GRAPH_API_VERSION}/{account_id}/media"),
                &[
                    ("image_url", image_url),
                    ("caption", caption),
                    ("access_token", token),
                ],
            )
            .await?;
        self.graph_post(
            &format!("https://graph.facebook.com/{GRAPH_API_VERSION}/{account_id}/media_publish"),
            &[("creation_id", &creation_id), ("access_token", token)],
        )
        .await
    }

    /// The connected Instagram Professional account's id.
    ///
    /// Instagram publishing runs against the IG user id, which is a different
    /// identifier from the Page id even though one token covers both.
    async fn instagram_account_id(&self) -> Result<Option<String>, SocialPostExecutorError> {
        let account_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT provider_account_id
            FROM fanbase_connections
            WHERE workspace_id = $1
              AND platform = 'instagram'
              AND status = 'connected'
              AND provider_account_id IS NOT NULL
            ORDER BY updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        Ok(account_id)
    }

    /// The image to publish next: the tenant's own photo assets, least
    /// recently published first.
    ///
    /// Rotation rather than "the newest photo", because posting the same
    /// picture every time is what a bot looks like — and the publish guard
    /// cannot catch it, since it compares captions and the caption changes.
    /// A photo that has never been published sorts first.
    ///
    /// Only `photo` and `logo` assets, only active ones, and only from this
    /// workspace: the point of the selector is that a model cannot introduce
    /// an image, so it reads rows an operator curated and nothing else.
    async fn next_instagram_image(&self) -> Result<Option<String>, SocialPostExecutorError> {
        let image_url: Option<String> = sqlx::query_scalar(
            r#"
            SELECT asset.url
            FROM viryaos_beacon_press_assets AS asset
            LEFT JOIN LATERAL (
                SELECT max(post.posted_at) AS last_published_at
                FROM social_posts AS post
                WHERE post.workspace_id = asset.workspace_id
                  AND post.platform = 'instagram'
                  AND post.status = 'posted'
                  AND post.image_url = asset.url
            ) AS use ON true
            WHERE asset.workspace_id = $1
              AND asset.active
              AND asset.asset_kind IN ('photo', 'logo')
              AND asset.url ~* '^https://'
            ORDER BY use.last_published_at ASC NULLS FIRST,
                     asset.sort_order,
                     asset.asset_key
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(image_url)
    }

    /// Publishes a drafted post to the tenant's own Facebook Page.
    ///
    /// The Page id is the connection's `provider_account_id` — the same one
    /// `growth_metric_sync` reads Page metrics from, so publishing and
    /// measuring cannot drift onto different Pages.
    ///
    /// Every refusal path holds the draft rather than failing it: a missing
    /// token, a missing connection, a guard verdict and a Graph API refusal
    /// all leave a post a person can still publish, with the reason recorded.
    async fn publish_to_facebook_page(
        &self,
        action: &ClaimedAction,
    ) -> Result<(), SocialPostExecutorError> {
        let Some(token) = self.facebook_page_access_token.as_ref() else {
            self.hold_for_human(action.id, "no facebook page access token is configured")
                .await?;
            return Ok(());
        };
        let Some(page_id) = self.facebook_page_id().await? else {
            self.hold_for_human(action.id, "no connected facebook page to post to")
                .await?;
            return Ok(());
        };
        let body = action.text.as_deref().unwrap_or("").trim();
        if body.is_empty() {
            self.hold_for_human(action.id, "the draft has no text")
                .await?;
            return Ok(());
        }

        // The read a person used to do before a post went out under the
        // band's name. A held post lands in the operator queue with its
        // reason, so the worst case of automatic mode is the behaviour that
        // preceded it.
        let recent = self.recent_content_hashes("facebook").await?;
        let verdict = review_outbound_post(
            body,
            &PublishContext {
                channel: PublishChannel::Social,
                approved_origins: &[self.public_origin.as_str()],
                recent_content_hashes: &recent,
            },
        );
        if let Some(reason) = verdict.hold_reason() {
            tracing::info!(
                action_id = %action.action_id,
                reason = reason.as_str(),
                "facebook post held for an operator by the publish guard"
            );
            self.hold_for_human(action.id, reason.as_str()).await?;
            return Ok(());
        }

        match self.submit_to_facebook_page(&page_id, body, token).await {
            Ok(post_id) => {
                // Same one-commit shape as the Instagram arm: post +
                // re-anchored measurement windows land or neither does.
                let mut posted_tx = self.pool.begin().await?;
                sqlx::query(
                    r#"
                    UPDATE social_posts
                    SET status = 'posted',
                        platform_post_url = $3,
                        posted_at = now(),
                        updated_at = now(),
                        error_message = NULL
                    WHERE workspace_id = $1 AND id = $2
                    "#,
                )
                .bind(self.workspace_id.into_uuid())
                .bind(action.id)
                .bind(format!("https://www.facebook.com/{post_id}"))
                .execute(&mut *posted_tx)
                .await?;
                crowdrelay_infra::fanbase::anchor_content_measurements_to_publication(
                    &mut posted_tx,
                    self.workspace_id.into_uuid(),
                    "social_posts",
                    action.id,
                )
                .await?;
                posted_tx.commit().await?;
                tracing::info!(
                    action_id = %action.action_id,
                    post_id = %post_id,
                    "facebook page post published"
                );
                Ok(())
            }
            // A refusal is a fact about the credential or the content, not a
            // transient failure, so it holds rather than retries. The most
            // likely one is the Page token lacking `pages_manage_posts`, and
            // retrying that forever would bury it.
            Err(SocialPostExecutorError::GraphRefused(message)) => {
                tracing::warn!(
                    action_id = %action.action_id,
                    error = %message,
                    "facebook refused the post; holding it for an operator"
                );
                self.hold_for_human(action.id, &format!("facebook refused the post: {message}"))
                    .await?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// POSTs the message to the Page feed and returns the created post id.
    async fn submit_to_facebook_page(
        &self,
        page_id: &str,
        message: &str,
        token: &str,
    ) -> Result<String, SocialPostExecutorError> {
        self.graph_post(
            &format!("https://graph.facebook.com/{GRAPH_API_VERSION}/{page_id}/feed"),
            &[("message", message), ("access_token", token)],
        )
        .await
    }

    /// One Graph API write, returning the id it created.
    ///
    /// Shared by the Page feed and both Instagram steps so there is one place
    /// that decides what a Graph response means. A refusal carries Meta's own
    /// message: `(#200) Requires pages_manage_posts permission` is something an
    /// operator can act on and "posting failed" is not.
    ///
    /// Form body rather than query string throughout — the token is a
    /// credential, and a URL is the one part of a request proxies log.
    async fn graph_post(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<String, SocialPostExecutorError> {
        #[derive(serde::Deserialize)]
        struct GraphResponse {
            id: Option<String>,
            error: Option<GraphError>,
        }
        #[derive(serde::Deserialize)]
        struct GraphError {
            message: String,
        }

        let response = self
            .http_client
            .post(url)
            .form(form)
            .send()
            .await
            .map_err(SocialPostExecutorError::GraphRequest)?;
        let parsed: GraphResponse = response
            .json()
            .await
            .map_err(SocialPostExecutorError::GraphRequest)?;
        if let Some(error) = parsed.error {
            return Err(SocialPostExecutorError::GraphRefused(error.message));
        }
        parsed.id.ok_or_else(|| {
            SocialPostExecutorError::GraphRefused(
                "the graph api returned neither an id nor an error".to_owned(),
            )
        })
    }

    /// The connected Facebook Page's id, if the tenant has one.
    async fn facebook_page_id(&self) -> Result<Option<String>, SocialPostExecutorError> {
        let page_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT provider_account_id
            FROM fanbase_connections
            WHERE workspace_id = $1
              AND platform = 'facebook'
              AND status = 'connected'
              AND provider_account_id IS NOT NULL
            ORDER BY updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        Ok(page_id)
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
