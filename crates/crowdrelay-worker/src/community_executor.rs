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
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::action_class::{ActionClass, effective_authority};
use crowdrelay_domain::autonomy::AutonomyLevel;
use crowdrelay_domain::publish_guard::{
    PublishChannel, PublishContext, content_hash, review_outbound_post,
};
use crowdrelay_domain::standing_approval::{
    StandingGrant, UnattendedAuthority, unattended_authority,
};
use crowdrelay_infra::reddit_proxy::read_reddit_proxy_from_db;
use serde::Deserialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

mod marks;
mod owned_replies;
mod relay;
mod replies;
mod standing;
use time::OffsetDateTime;

use relay::RelayBatchGate;

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
/// Maximum response body size for HTTP calls to the agents service and
/// Reddit's public JSON endpoint. 64 KiB is generous for the JSON payloads
/// these endpoints return (post submissions, metrics, comment listings)
/// while preventing a runaway response from spiking memory.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// Checks the Content-Length header and returns an error if the response
/// exceeds [`MAX_RESPONSE_BYTES`]. For responses without Content-Length
/// (chunked), the caller must use a bounded read.
fn check_response_size(response: &reqwest::Response) -> Result<(), CommunityExecutorError> {
    if let Some(length) = response.content_length()
        && length > MAX_RESPONSE_BYTES
    {
        return Err(CommunityExecutorError::RedditApi(format!(
            "response exceeds size limit: {length} bytes (max {MAX_RESPONSE_BYTES})"
        )));
    }
    Ok(())
}

/// Builds a reqwest client with an optional proxy. Shared between the
/// constructor and the run-loop proxy refresh.
fn build_http_client(
    proxy_url: Option<&str>,
    operation_timeout: Duration,
    user_agent: &str,
) -> Result<reqwest::Client, CommunityExecutorError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(operation_timeout.min(Duration::from_secs(10)))
        .timeout(operation_timeout)
        .user_agent(user_agent);
    if let Some(proxy) = proxy_url {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|e| CommunityExecutorError::RedditApi(format!("invalid proxy URL: {e}")))?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(CommunityExecutorError::ClientBuild)
}

// The 24h post ceiling is earned, not fixed: one a day until posts have
// survived, and back to one on any removal. See `standing` and
// `crowdrelay_domain::reddit_standing`.

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

/// How long to wait before reconsidering a draft held by the subreddit cooldown.
///
/// The cooldown is seven days, so the ten-minute window above would re-check
/// the same draft a thousand times and log a thousand deferrals. Six hours is
/// short enough that the draft goes out promptly once the window opens.
const SUBREDDIT_COOLDOWN_BACKOFF: Duration = Duration::from_secs(6 * 60 * 60);

/// How many times a transient failure may defer a draft before it is given up on.
///
/// A transport error, a 5xx from the agents service, or a browser session that
/// has expired are all conditions that a later cycle may find resolved. They used
/// to call `mark_failed`, which discarded the draft and propagated failure to the
/// parent autopilot action — and the action is terminal, so "the operator can
/// re-approve" was not available: the brain will not draft the same community
/// again for seven days. One bad credential could therefore consume every queued
/// draft in a single cycle.
///
/// Six, so a draft survives roughly an hour of an unavailable agents service at
/// the ten-minute retry window, and still stops rather than retrying forever.
const MAX_TRANSIENT_ATTEMPTS: i32 = 6;

/// How long after a post was created do we keep polling its metrics.
/// After this window, engagement is considered stale and polling stops.
const METRICS_WINDOW: Duration = Duration::from_secs(72 * 60 * 60);

/// Poll interval: 30 min while a post is under 6h old (when removals land),
/// then 3h — ~34 reads a post through the one session, not ~288.
const METRICS_POLL_MIN_INTERVAL: Duration = Duration::from_secs(30 * 60);
const METRICS_POLL_SETTLED_INTERVAL: Duration = Duration::from_secs(3 * 60 * 60);

/// Maximum posts to poll for metrics in a single cycle. Bounded to keep
/// the cycle fast even if many posts are in the window.
const METRICS_POLL_BATCH: i64 = 10;

/// Reddit requires a descriptive User-Agent following their guideline:
/// `<platform>:<app ID>:<version string> (by /u/<username>)`. The username is
/// the tenant's own Reddit identity — `CROWDRELAY_REDDIT_USERNAME` when the
/// tenant has one configured, else the workspace slug (which at least names
/// the tenant honestly to Reddit's ops team instead of another band's
/// account). Virya keeps its account name as the historical default.
fn reddit_user_agent(workspace_slug: &str) -> String {
    let username = std::env::var("CROWDRELAY_REDDIT_USERNAME")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            if workspace_slug == "virya" {
                "virya_band".to_owned()
            } else {
                workspace_slug.to_owned()
            }
        });
    format!("server:music.crowdrelay.community:v1.0.0 (by /u/{username})")
}

/// Public origin for smart link resolution. The smart_link stored in the
/// action payload is a `/l/{slug}` path; Reddit needs a full URL. The Virya
/// default is a back-compat shim — every other tenant must declare its own
/// `CROWDRELAY_PUBLIC_ORIGIN` rather than link posts to another band's site.
const DEFAULT_PUBLIC_ORIGIN: &str = "https://virya.music";

/// Reddit is read-only: this executor drafts posts and never publishes them.
///
/// The decision is about what Reddit access actually is, not about a
/// preference. Reddit requires authentication for its JSON API from every IP
/// — measured 403 from a datacenter host and a residential connection alike,
/// with `old.reddit.com` redirecting to login and the HTML page serving a
/// JavaScript proof-of-work challenge. API access for this account was
/// refused under Reddit's Responsible Builder Policy, so no script-app
/// credential exists. What remains is a headless browser holding a login
/// session, and that session is the only Reddit access the system has at all:
/// community observation, subreddit metrics and post engagement all run
/// through it.
///
/// Publishing through that same session is what puts it at risk. An automated
/// post that a moderator reads as spam does not cost a post — it costs the
/// account, and with it every read the growth loop depends on. Reading is
/// worth more than posting here, so posting does not happen automatically.
///
/// Drafting continues. `community_posts` rows are written
/// `awaiting_manual_post`, an operator publishes and registers the URL
/// through `POST /v1/control-plane/community-posts/{id}/register-manual`, and
/// measurement proceeds from there exactly as it would have.
///
/// Publishing therefore takes two switches, not one.
/// `CROWDRELAY_COMMUNITY_AUTO_POST=true` says "publish community posts", and
/// `CROWDRELAY_REDDIT_WRITE_ENABLED=true` says "and Reddit specifically is in
/// scope". Either one alone leaves this executor drafting.
///
/// Two, because a single flag is easy to set while copying an env file
/// between hosts, and one of these names Reddit explicitly. This was a
/// hardcoded constant for exactly that reason; it is a flag now because the
/// operator made the call deliberately, and the paragraphs above are what
/// they were deciding against.
///
/// Read-only by default. An unset variable, a typo and a fresh deployment all
/// mean the same thing: draft, do not publish.
fn reddit_write_enabled() -> bool {
    std::env::var("CROWDRELAY_REDDIT_WRITE_ENABLED")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "true" | "1" | "yes" | "on")
        })
        .unwrap_or(false)
}

#[derive(Debug, Error)]
pub enum CommunityExecutorError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("reddit API error: {0}")]
    RedditApi(String),
    /// A 5xx from the agents service: the login/session/browser layer failed
    /// before Reddit ever saw the post. Not a content refusal — the draft is
    /// intact and publishable once the session is repaired, so this routes to
    /// the deferral arm rather than `mark_failed`.
    #[error("reddit session unavailable: {0}")]
    SessionUnavailable(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no agents service configured for Reddit posting")]
    NoAgentsService,
    #[error("rate limited by Reddit")]
    RateLimited,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
    /// Startup configuration is missing and no tenant default exists — the
    /// worker refuses rather than send another tenant's links or identity.
    #[error("invalid community executor configuration: {0}")]
    Config(String),
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
    /// Reddit User-Agent, built once so a proxy-refresh rebuild sends the
    /// same tenant identity.
    user_agent: String,
    /// The Meta Page token the social-post sync already uses. Reposts carry
    /// the source's media, and the signed CDN URLs expire — at post time a
    /// fresh one is minted through `/{media_id}` under this credential.
    /// `None` means no re-mint: the stored URL is tried as-is.
    facebook_page_access_token: Option<String>,
}

impl CommunityExecutorWorker {
    /// Whether Reddit posting is disabled by policy rather than by
    /// configuration. Reported at startup so an operator who set
    /// `CROWDRELAY_COMMUNITY_AUTO_POST` is told plainly that it had no
    /// effect, instead of watching drafts pile up and wondering.
    #[must_use]
    pub fn reddit_is_read_only() -> bool {
        !reddit_write_enabled()
    }

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
        facebook_page_access_token: Option<String>,
    ) -> Result<Self, CommunityExecutorError> {
        let workspace_slug =
            std::env::var("CROWDRELAY_WORKSPACE_SLUG").unwrap_or_else(|_| "virya".to_owned());
        let user_agent = reddit_user_agent(&workspace_slug);
        let http_client = build_http_client(proxy_url.as_deref(), operation_timeout, &user_agent)?;
        let http_client = Arc::new(RwLock::new(http_client));
        let public_origin = match std::env::var("CROWDRELAY_PUBLIC_ORIGIN") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_owned(),
            _ if workspace_slug == "virya" => DEFAULT_PUBLIC_ORIGIN.to_owned(),
            _ => {
                return Err(CommunityExecutorError::Config(
                    "CROWDRELAY_PUBLIC_ORIGIN is required for non-Virya community execution"
                        .to_owned(),
                ));
            }
        };
        // Read-only wins over whatever the caller asked for. The guard lives
        // here rather than at the call site because this type owns the
        // invariant, and `new` is public.
        // Both switches, or it drafts. `manual_mode` already carries
        // `CROWDRELAY_COMMUNITY_AUTO_POST` being off; this adds the
        // Reddit-specific consent on top of it.
        let manual_mode = manual_mode || !reddit_write_enabled();
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
            user_agent,
            facebook_page_access_token,
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
                        match build_http_client(
                            new_proxy.as_deref(),
                            self.operation_timeout,
                            &self.user_agent,
                        ) {
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

        // Batches whose observation window closed become result rows — the
        // card answers "what did the week give us" instead of asking.
        self.finish_observed_batches().await?;

        let actions = self.claim_pending_actions().await?;
        let mut processed = 0;
        for action in actions {
            match self.process_action(&action).await {
                Ok(()) => processed += 1,
                Err(CommunityExecutorError::RateLimited) => {
                    // Rate limited — set status for later retry, don't fail.
                    if let Err(e) = self.mark_rate_limited(action.id, RATE_LIMIT_BACKOFF).await {
                        tracing::warn!(error = %e, "failed to mark rate_limited");
                    }
                }
                Err(CommunityExecutorError::NoAgentsService) => {
                    // Configuration, not content. Fixing the configuration makes
                    // the same draft publishable, so the draft is kept.
                    if let Err(e) = self
                        .mark_transient_failure(
                            action.id,
                            "no agents service configured for Reddit posting",
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "failed to defer no-agents-service");
                    }
                }
                Err(CommunityExecutorError::RedditApi(message)) => {
                    // Reddit itself refused the post. That is about the content or
                    // the account, and a retry would repeat it, so a person looks.
                    tracing::warn!(
                        action_id = %action.action_id,
                        subreddit = %action.subreddit,
                        error = %message,
                        "reddit refused the community post"
                    );
                    if let Err(e) = self.mark_failed(action.id, &message).await {
                        tracing::warn!(error = %e, "failed to mark reddit refusal");
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        action_id = %action.action_id,
                        subreddit = %action.subreddit,
                        error = %error,
                        "failed to post community engagement — deferring"
                    );
                    // A transport error, a 5xx, or an expired browser session may
                    // be resolved by the time the next cycle runs. `mark_failed`
                    // here discarded the draft and told the brain its post had
                    // failed, and the parent action is terminal — so one bad
                    // credential could consume every queued draft in a cycle.
                    let msg = error.to_string();
                    if let Err(e) = self.mark_transient_failure(action.id, &msg).await {
                        tracing::warn!(error = %e, "failed to defer error");
                    }
                }
            }
        }

        // Second phase: poll Reddit for post performance metrics on recently
        // posted content. This is the "eyes" of the growth loop — the system
        // learns which posts generate engagement.
        processed += self.poll_post_metrics().await?;
        processed += self.run_reply_lane().await?;
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
    ///
    /// Public so tests can drive the real recovery path instead of replaying
    /// its writes — a regression in the SQL here used to be invisible to a
    /// test that repeated the statements by hand.
    pub async fn recover_stale_posting(&self) -> Result<(), CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();

        // The post failure, the action's transition to `unknown`, and the
        // assignment update commit in one transaction — a crash between the
        // statements used to leave posts=failed next to actions=succeeded
        // with no sweep able to reconcile the contradiction.
        let mut tx = self.pool.begin().await?;

        // Step 1: Find stale posting rows and collect their action_ids.
        // FOR UPDATE SKIP LOCKED keeps a second worker from racing the same
        // recovery window.
        let stale_rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            r#"
            SELECT id, action_id FROM community_posts
            WHERE workspace_id = $1
              AND status = 'posting'
              AND updated_at < now() - make_interval(secs => $2::double precision)
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(ws)
        .bind(POSTING_STALE_THRESHOLD.as_secs() as i64)
        .fetch_all(&mut *tx)
        .await?;

        if stale_rows.is_empty() {
            tx.commit().await?;
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
              AND workspace_id = $3
            "#,
        )
        .bind(&post_ids)
        .bind(format!(
            "{CRASH_POSTING_ERROR_PREFIX} — check Reddit manually"
        ))
        .bind(ws)
        .execute(&mut *tx)
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
            .execute(&mut *tx)
            .await?;

            // Step 4: Transition experiment assignments to 'unknown'.
            // Unknown is excluded from both realized-treatment and
            // failed-treatment counts. It can later resolve to 'executed'
            // or 'failed' via reconciliation.
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
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

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
    ///
    /// Public so tests can drive the real claim — a pacing regression in
    /// this query is what turns a one-per-hour drip into a burst, and a
    /// test replaying the statements by hand would not see it.
    pub async fn claim_pending_actions(
        &self,
    ) -> Result<Vec<ClaimedAction>, CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let mut tx = self.pool.begin().await?;

        // Guardrail 1: 24h post limit. If the workspace has already posted its
        // earned daily cap in the last 24 hours, don't claim any more.
        // Checking this inside the transaction prevents the "posting → failed"
        // transition that would otherwise briefly make an action look active.
        //
        // Campaign deliveries (relay_source_id set) are exempt and uncounted:
        // an operator-approved drip of one post per batch interval cannot run
        // under a one-per-day cap — fifty communities would take fifty days —
        // and must not starve uncampaigned drafts either. Their pacing is the
        // batch interval in the claim predicate below.
        let recent_posts: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM community_posts
            WHERE workspace_id = $1
              AND status = 'posted'
              AND posted_at > now() - INTERVAL '24 hours'
              AND relay_source_id IS NULL
            "#,
        )
        .bind(ws)
        .fetch_one(&mut *tx)
        .await?;
        // The cap binds uncampaigned deliveries only. Campaign rows pace
        // themselves on their batch interval, so the cap becoming reached
        // closes the uncampaigned lane without parking the drip.
        let history = standing::post_history(&mut *tx, ws).await?;
        let cap_reached = recent_posts >= standing::daily_cap(&history);

        // Step 1: Insert pending rows for unprocessed succeeded actions.
        //
        // The community must be a screened-and-admitted target: the action
        // payload's subreddit came from a model and target_id needed only to
        // parse as a UUID, so a fabricated or unvetted target used to write a
        // real post row here. The outcome-ingest gate rejects those outcomes
        // upstream; this predicate is the second wall for any action that
        // reaches `succeeded` by another path.
        sqlx::query(
            r#"
            INSERT INTO community_posts
                (workspace_id, action_id, target_id, subreddit, title, body, smart_link,
                 image_url, media_id, source_url, relay_source_id, status)
            SELECT
                $1,
                a.id,
                CASE WHEN a.payload->>'target_id' ~ '^[0-9a-fA-F-]{36}$'
                     THEN (a.payload->>'target_id')::uuid END,
                COALESCE(a.payload->>'subreddit', ''),
                COALESCE(a.payload->>'title', ''),
                COALESCE(a.payload->>'body', ''),
                a.payload->>'smart_link',
                a.payload->>'image_url',
                a.payload->>'media_id',
                a.payload->>'source_url',
                -- The batch key: a delivery drafted under a relay batch is
                -- paced by the batch's own interval instead of the workspace
                -- cap — the operator approved the spread as one campaign.
                CASE WHEN a.payload->>'source_id' ~ '^[0-9a-fA-F-]{36}$'
                     THEN (a.payload->>'source_id')::uuid END,
                -- A delivery whose batch was answered while its action was
                -- mid-flight lands dead, not pending: revoke is a content
                -- veto, and a post seeded under a closed batch would sit
                -- claimable-in-name forever since the claim lane refuses
                -- closed batches.
                CASE WHEN a.payload->>'source_id' ~ '^[0-9a-fA-F-]{36}$'
                          AND EXISTS (
                              SELECT 1 FROM community_relay_batches rb
                              WHERE rb.workspace_id = a.workspace_id
                                AND rb.source_id = (a.payload->>'source_id')::uuid
                                AND rb.status IN ('revoked', 'done')
                          )
                     THEN 'cancelled' ELSE 'pending' END
            FROM autopilot_actions a
            WHERE a.workspace_id = $1
              AND a.action_kind = 'community.engage.request'
              AND a.status = 'succeeded'
              AND a.payload->>'target_id' ~ '^[0-9a-fA-F-]{36}$'
              AND EXISTS (
                  SELECT 1 FROM agent_outreach_targets t
                  WHERE t.workspace_id = a.workspace_id
                    AND t.id = CASE WHEN a.payload->>'target_id' ~ '^[0-9a-fA-F-]{36}$'
                                    THEN (a.payload->>'target_id')::uuid END
                    AND t.target_kind = 'community'
                    AND t.screening_verdict = 'admitted'
                    AND t.status = 'promoted'
                    AND normalize_subreddit(t.subreddit) =
                        normalize_subreddit(a.payload->>'subreddit')
              )
              AND NOT EXISTS (
                  SELECT 1 FROM community_posts cp WHERE cp.action_id = a.id
              )
            ON CONFLICT (action_id) DO NOTHING
            "#,
        )
        .bind(ws)
        .execute(&mut *tx)
        .await?;

        // Step 2: Fail rows whose community stopped being admitted between
        // seed and claim — a demoted or re-screened target must not receive
        // the post it was queued for. Without this the row would sit in
        // `pending` forever: the seed-time EXISTS gate does not revisit it.
        // The parent actions are told in the same transaction — an
        // unreported veto leaves the ledger claiming a send that was
        // refused here.
        let vetoed_actions: Vec<Option<Uuid>> = sqlx::query_scalar(
            r#"
            UPDATE community_posts cp
            SET status = 'failed',
                error_message = 'community no longer an admitted target at post time',
                updated_at = now()
            WHERE cp.workspace_id = $1
              AND cp.status IN ('pending', 'rate_limited', 'awaiting_manual_post')
              AND NOT EXISTS (
                  SELECT 1 FROM agent_outreach_targets t
                  WHERE t.workspace_id = cp.workspace_id
                    AND t.id = cp.target_id
                    AND t.target_kind = 'community'
                    AND t.screening_verdict = 'admitted'
                    AND t.status = 'promoted'
              )
            RETURNING cp.action_id
            "#,
        )
        .bind(ws)
        .fetch_all(&mut *tx)
        .await?;
        let vetoed_actions: Vec<Uuid> = vetoed_actions.into_iter().flatten().collect();
        if !vetoed_actions.is_empty() {
            sqlx::query(
                r#"
                UPDATE autopilot_actions
                SET status = 'failed',
                    finished_at = now(),
                    last_error_kind = 'community_target_not_admitted',
                    updated_at = now()
                WHERE id = ANY($1) AND workspace_id = $2 AND status = 'succeeded'
                "#,
            )
            .bind(&vetoed_actions)
            .bind(ws)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                r#"
                UPDATE experiment_assignments AS ea
                SET execution_status = 'failed',
                    trace_id = COALESCE(ea.trace_id, (SELECT trace_id FROM autopilot_actions WHERE id = ea.action_id))
                WHERE ea.workspace_id = $1
                  AND ea.action_id = ANY($2)
                  AND ea.execution_status = 'dispatched'
                "#,
            )
            .bind(ws)
            .bind(&vetoed_actions)
            .execute(&mut *tx)
            .await?;
        }

        // Step 3: Claim pending and rate_limited (past backoff) rows.
        // Transition them to `posting` atomically.
        // Guardrail 2: exclude rows whose subreddit is on cooldown (a post
        // to that subreddit was made in the last SUBREDDIT_COOLDOWN_DAYS days).
        let rows = sqlx::query_as::<_, ClaimedAction>(
            r#"
            WITH target AS (
                SELECT id, status AS claimed_from FROM community_posts
                WHERE id IN (
                    SELECT c.id
                    FROM community_posts c
                    -- One row per relay batch per sweep, not one per
                    -- eligibility: every pending delivery of a batch passes
                    -- its interval check while the batch has no recent post,
                    -- so a flat claim could take five rows of one batch and
                    -- post them back-to-back — the burst the interval exists
                    -- to prevent. DISTINCT ON cannot take FOR UPDATE, so the
                    -- dedup runs lock-free here and the lock happens on the
                    -- outer select.
                    JOIN (
                        SELECT DISTINCT ON (
                            COALESCE(c2.relay_source_id::text, c2.id::text)
                        ) c2.id
                        FROM community_posts c2
                        LEFT JOIN community_relay_batches batch
                            ON batch.workspace_id = c2.workspace_id
                           AND batch.source_id = c2.relay_source_id
                        WHERE c2.workspace_id = $1
                          AND (
                              c2.status = 'pending'
                              OR (c2.status = 'rate_limited'
                                  AND c2.rate_limited_until IS NOT NULL
                                  AND c2.rate_limited_until < now())
                              -- Drafts manual mode wrote, adopted once
                              -- publishing is on. Without this clause,
                              -- turning autopilot on moved nothing:
                              -- `awaiting_manual_post` was in no claim
                              -- predicate, the parent action was already
                              -- consumed, and the seven-day cooldown stopped
                              -- the brain from drafting the community again.
                              -- Five ready drafts for communities of 2.6M and
                              -- 1M members were stranded permanently by a
                              -- missing status in one WHERE clause.
                              --
                              -- $3 is false in manual mode, so nothing is
                              -- adopted while a person is still the publisher.
                              OR (c2.status = 'awaiting_manual_post' AND $3)
                          )
                          AND EXISTS (
                              SELECT 1 FROM agent_outreach_targets t
                              WHERE t.workspace_id = c2.workspace_id
                                AND t.id = c2.target_id
                                AND t.target_kind = 'community'
                                AND t.screening_verdict = 'admitted'
                                AND t.status = 'promoted'
                          )
                          AND NOT EXISTS (
                              SELECT 1 FROM community_posts recent
                              WHERE recent.workspace_id = $1
                                AND normalize_subreddit(recent.subreddit) =
                                    normalize_subreddit(c2.subreddit)
                                AND recent.status = 'posted'
                                AND recent.posted_at > now() - make_interval(days => $2)
                          )
                          -- Two lanes. An uncampaigned delivery obeys the
                          -- workspace 24h cap ($4). A campaign delivery obeys
                          -- its batch: the batch must be live-approved, and
                          -- the gap since the batch's last posted row must
                          -- clear the interval the operator approved. The gap
                          -- is measured, not scheduled — a worker that slept
                          -- five hours posts one, not five.
                          AND (
                              c2.relay_source_id IS NULL AND NOT $4
                              OR (
                                  c2.relay_source_id IS NOT NULL
                                  -- A revoked or finished batch is a veto on
                                  -- the content itself — no standing answer,
                                  -- grant included, may carry it past that.
                                  -- A missing batch row (LEFT JOIN miss)
                                  -- means the delivery's provenance broke;
                                  -- it waits rather than posts.
                                  AND batch.status IN ('awaiting_approval', 'approved')
                                  AND (
                                      batch.status = 'approved'
                                      -- The card hasn't answered yet, but a
                                      -- live standing grant for this one
                                      -- community already did. `is_live`'s
                                      -- rule — granted class, unrevoked,
                                      -- unexpired — is inlined here because
                                      -- a join predicate cannot call it;
                                      -- the table's CHECK already confines
                                      -- action_class to grantable values.
                                      OR EXISTS (
                                          SELECT 1 FROM standing_approvals sg
                                          WHERE sg.workspace_id = c2.workspace_id
                                            AND sg.action_kind = 'community.engage.request'
                                            AND sg.target_key = c2.target_id::text
                                            AND sg.revoked_at IS NULL
                                            AND sg.expires_at > now()
                                            -- A grant is an answer only while
                                            -- the question it answered is still
                                            -- asked: the outreach context must
                                            -- sit at require_approval or higher
                                            -- and the class ceiling no stricter.
                                            -- Dialled to observe/recommend, the
                                            -- row stays parked — claiming it
                                            -- would only repark and burn an
                                            -- attempt on nothing attempted.
                                            AND EXISTS (
                                                SELECT 1
                                                FROM autopilot_policies p
                                                WHERE p.workspace_id = c2.workspace_id
                                                  AND p.context = 'outreach'
                                                  AND p.autonomy_level IN
                                                      ('require_approval', 'bounded_auto')
                                            )
                                            AND NOT EXISTS (
                                                SELECT 1
                                                FROM growth_autonomy g
                                                WHERE g.workspace_id = c2.workspace_id
                                                  AND g.action_class = sg.action_class
                                                  AND g.ceiling IN ('observe', 'recommend')
                                            )
                                      )
                                  )
                                  AND NOT EXISTS (
                                      SELECT 1 FROM community_posts last
                                      WHERE last.workspace_id = c2.workspace_id
                                        AND last.relay_source_id = c2.relay_source_id
                                        AND last.status = 'posted'
                                        AND last.posted_at >
                                            now() - make_interval(secs => batch.interval_seconds)
                                  )
                              )
                          )
                        -- The oldest due delivery per batch; uncampaigned rows
                        -- each form their own group on `id`, so the lane split
                        -- does not collapse them. Fresh work ranks ahead of
                        -- held drafts: a publish-guard hold re-adopts and
                        -- re-holds deterministically, and if it also sat first
                        -- by age the batch's one-per-sweep slot churned on it
                        -- forever while every pending sibling starved.
                        ORDER BY COALESCE(c2.relay_source_id::text, c2.id::text),
                                 (c2.status = 'awaiting_manual_post'),
                                 c2.created_at
                    ) pick ON pick.id = c.id
                    ORDER BY c.created_at
                    LIMIT 5
                    FOR UPDATE OF c SKIP LOCKED
                )
            ), claimed AS (
                UPDATE community_posts AS cp
                SET status = 'posting',
                    attempts = cp.attempts + 1,
                    updated_at = now()
                FROM target
                WHERE cp.id = target.id
                RETURNING cp.id, cp.action_id, cp.target_id, cp.subreddit, cp.title,
                          cp.body, cp.smart_link, cp.image_url, cp.media_id,
                          cp.source_url, cp.relay_source_id, target.claimed_from
            )
            SELECT c.id, c.action_id, c.target_id, c.subreddit, c.title, c.body,
                   c.smart_link, c.image_url, c.media_id, c.source_url,
                   c.relay_source_id, c.claimed_from,
                   a.trace_id, a.causation_id, a.decision_id
            FROM claimed c
            LEFT JOIN autopilot_actions a ON a.id = c.action_id
            "#,
        )
        .bind(ws)
        .bind(SUBREDDIT_COOLDOWN_DAYS)
        .bind(!self.manual_mode)
        .bind(cap_reached)
        .fetch_all(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(rows)
    }

    /// Processes a single claimed action: checks anti-spam guardrails,
    /// posts to Reddit via the agents service browser, and records the result.
    async fn process_action(&self, action: &ClaimedAction) -> Result<(), CommunityExecutorError> {
        if action.claimed_from == "awaiting_manual_post" {
            tracing::warn!(
                post_id = %action.id,
                subreddit = %action.subreddit,
                "adopting a draft that was waiting for manual publication — if it \
                 was already published by hand without registering the URL, this \
                 will post it a second time"
            );
        }
        // Anti-spam: check subreddit cooldown.
        if self.subreddit_on_cooldown(&action.subreddit).await? {
            tracing::info!(
                subreddit = %action.subreddit,
                "subreddit on 7-day cooldown, deferring"
            );
            // Deferred, not failed. Our own cooldown saying "not yet" is not a
            // post that failed: the draft is intact and publishable, and
            // `mark_failed` both discarded it and propagated failure to the
            // parent autopilot action — so the brain learned that a post it
            // wrote had failed when nothing had been attempted.
            self.mark_rate_limited(action.id, SUBREDDIT_COOLDOWN_BACKOFF)
                .await?;
            return Ok(());
        }

        // Anti-spam: check 24h rate limit. Campaign deliveries are exempt —
        // the operator approved the batch's drip; their guardrail is the
        // per-batch interval the claim already enforced.
        if action.relay_source_id.is_none() && self.rate_limit_reached().await? {
            tracing::info!("24h post limit reached, deferring");
            // Same reason as the cooldown above. `claim_pending_actions`
            // already refuses to claim anything while the cap is reached, and
            // says so in its own comment, so this is the narrow race: the cap
            // becoming reached between the claim and the attempt, which is
            // reachable because they are separate transactions. Rare, and
            // `mark_failed` made it expensive — a draft nobody attempted was
            // discarded and the brain was told its post had failed.
            self.mark_rate_limited(action.id, RATE_LIMIT_BACKOFF)
                .await?;
            return Ok(());
        }

        // The batch's own cadence, re-checked at send time. The claim takes
        // at most one due row per batch per sweep, but the check that
        // mattered — "did this batch post inside its interval" — was answered
        // before a sibling's send recorded `posted`. Rechecking here, after
        // the claim and before Reddit, is what turns "usually hourly" into
        // "never sooner than the interval".
        if let Some(source_id) = action.relay_source_id {
            match self.relay_batch_gate(source_id, action.target_id).await? {
                RelayBatchGate::Open => {}
                RelayBatchGate::Defer(remaining) => {
                    tracing::info!(
                        post_id = %action.id,
                        source_id = %source_id,
                        "relay batch interval not yet elapsed, deferring"
                    );
                    self.mark_rate_limited(action.id, remaining).await?;
                    return Ok(());
                }
                RelayBatchGate::Parked => {
                    // The authority this delivery was claimed under is gone —
                    // grant revoked or the context dialled below a question a
                    // grant may answer — and the batch card itself hasn't
                    // answered. Back to pending, where the claim leaves it
                    // until the card or a grant says yes. The claim's
                    // `attempts` bump is refunded: nothing was attempted, and
                    // a context flip-flopping between sweeps must not burn
                    // the transient-failure budget on parked rows.
                    tracing::info!(
                        post_id = %action.id,
                        source_id = %source_id,
                        "grant no longer covers the target and the batch is unanswered — reparking"
                    );
                    sqlx::query(
                        "UPDATE community_posts \
                         SET status = 'pending', \
                             attempts = attempts - 1, \
                             updated_at = now() \
                         WHERE id = $1 AND workspace_id = $2",
                    )
                    .bind(action.id)
                    .bind(self.workspace_id.into_uuid())
                    .execute(&self.pool)
                    .await?;
                    return Ok(());
                }
                RelayBatchGate::Closed => {
                    // The batch's answer changed between claim and send —
                    // a revoke lands exactly here. `pending` rows were
                    // cancelled by the revoke itself; this one was already
                    // claimed, so it is cancelled here rather than left to
                    // be claimed again under a batch that is not approved.
                    tracing::info!(
                        post_id = %action.id,
                        source_id = %source_id,
                        "relay batch is no longer approved — cancelling the delivery"
                    );
                    sqlx::query(
                        "UPDATE community_posts \
                         SET status = 'cancelled', updated_at = now() \
                         WHERE id = $1 AND workspace_id = $2",
                    )
                    .bind(action.id)
                    .bind(self.workspace_id.into_uuid())
                    .execute(&self.pool)
                    .await?;
                    return Ok(());
                }
            }
        }

        // Manual mode: skip Reddit API, mark as awaiting manual post.
        // The operator posts manually and registers the URL via the API.
        if self.manual_mode {
            sqlx::query(
                r#"
                UPDATE community_posts
                SET status = 'awaiting_manual_post',
                    updated_at = now()
                WHERE id = $1 AND workspace_id = $2
                "#,
            )
            .bind(action.id)
            .bind(self.workspace_id.into_uuid())
            .execute(&self.pool)
            .await?;
            tracing::info!(
                action_id = %action.action_id,
                subreddit = %action.subreddit,
                "community post marked as awaiting manual post"
            );
            return Ok(());
        }

        // Not while the community sleeps, and never on the dot of approval.
        if self.defer_to_posting_window(action).await? {
            return Ok(());
        }
        // Standing (halted, a community that removed us), register, and the
        // independent review: any of them sends the draft to a person.
        if let Some(reason) = self.standing_hold(action).await? {
            self.hold_for_human(action.id, &reason).await?;
            return Ok(());
        }

        // Browser-only: the agents service posts through a real logged-in
        // browser session — the only Reddit access path that works reliably.
        // No OAuth fallback: if the agents service is unavailable, the post
        // fails and the operator can re-approve.
        if self.agent_service_auth_key.is_none() {
            return Err(CommunityExecutorError::NoAgentsService);
        }

        // The publish guard: the same mechanical review the owned channels
        // run before a post leaves. This executor used to skip it — the one
        // channel where the post lands in someone else's community was the
        // one channel nothing re-read. A hold parks the draft for a person
        // (`awaiting_manual_post`) with the reason; nothing is lost and
        // nothing unreviewed goes out.
        let post_body = self.build_post_body(&action.body, action.smart_link.as_deref());
        // The guard reads what a reader sees: the title is the message on
        // every rung of the format ladder (the drafted body may be empty on
        // an image post), so the review covers both.
        let post_text = format!("{}\n\n{}", action.title, post_body);
        let recent_hashes = self.recent_posted_hashes().await?;
        let approved_origins = self.community_approved_origins();
        let approved_origin_refs: Vec<&str> = approved_origins.iter().map(String::as_str).collect();
        let verdict = review_outbound_post(
            &post_text,
            &PublishContext {
                channel: PublishChannel::Community,
                approved_origins: &approved_origin_refs,
                approved_links: &[],
                recent_content_hashes: &recent_hashes,
                // Stored hashes cover `community_posts.body` — the raw draft
                // body. The reviewed text adds the title and the smart link
                // the dispatch appended, so it could never match them.
                dedupe_text: Some(&action.body),
            },
        );
        if let Some(reason) = verdict.hold_reason() {
            tracing::info!(
                action_id = %action.action_id,
                reason = reason.as_str(),
                "community post held for an operator by the publish guard"
            );
            self.hold_for_human(action.id, reason.as_str()).await?;
            return Ok(());
        }

        // The picture, fresh if we can re-mint it. Stored media URLs are
        // signed CDN links that expire between sync and post time; the
        // Graph id re-mints a working one. Both media and the source
        // permalink go to the agents service — the submit ladder there is
        // image → link → self, so a subreddit that refuses image posts
        // still gets the repost as a link rather than a failure.
        let image_url = self.resolve_image_url(action).await;

        // On the link rung the body is not rendered — the submitted URL is
        // the post's only clickable. When the draft carries a tracked link,
        // submit that instead of the bare permalink: it resolves to the same
        // destination through `/l/`, so the audience ends up in the same
        // place and the click is counted.
        let tracked_link_url = self.tracked_link_url(action.smart_link.as_deref());
        let reddit_result = self
            .submit_via_agent_browser(
                action,
                &post_body,
                image_url.as_deref(),
                tracked_link_url.as_deref().or(action.source_url.as_deref()),
            )
            .await?;

        // Record success, and re-anchor the action's pending measurements to
        // `posted_at` in the same transaction: the exposure window starts
        // when the audience could actually see the post, not when the row
        // was claimed — a draft that sat in `rate_limited` for two days does
        // not get a fourteen-day window that was half over before anyone
        // could see it. Same re-anchor the manual-registration path runs;
        // measurements stay pending if the process dies before the commit.
        let mut posted_tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            UPDATE community_posts
            SET status = 'posted',
                reddit_post_id = $2,
                reddit_post_url = $3,
                post_kind = $4,
                posted_at = now(),
                updated_at = now(),
                error_message = NULL,
                rate_limited_until = NULL
            WHERE id = $1 AND workspace_id = $5
            "#,
        )
        .bind(action.id)
        .bind(&reddit_result.post_id)
        .bind(&reddit_result.post_url)
        .bind(&reddit_result.kind)
        .bind(self.workspace_id.into_uuid())
        .execute(&mut *posted_tx)
        .await?;
        crowdrelay_infra::fanbase::anchor_measurements_to_publication(
            &mut *posted_tx,
            self.workspace_id.into_uuid(),
            action.id,
        )
        .await?;
        // The reach row and the assignment transition belong to the same
        // commit as the post itself: a crash between "post marked posted"
        // and these writes would leave an assignment `dispatched` forever —
        // the claim query never revisits a `posted` row and no sweep owns
        // it. Both writes are idempotent (`ON CONFLICT DO NOTHING`, a
        // monotonic from-guard), so rolling them into `posted_tx` only
        // shrinks the window where the record can lie.
        sqlx::query(r#"INSERT INTO reach_events (workspace_id, action_id, recipient_kind, recipient_id, channel, template_id, estimated_reach, status, metadata, trace_id, causation_id) VALUES ($1, $2, 'subreddit_audience', $3, 'reddit_post', 'community-engager', $5, 'delivered', jsonb_build_object('subreddit', $3, 'post_url', $4), $6, $2) ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL DO NOTHING"#).bind(self.workspace_id.into_uuid()).bind(action.action_id).bind(&action.subreddit).bind(&reddit_result.post_url).bind(100_i32).bind(action.trace_id).execute(&mut *posted_tx).await?; // reach ledger — estimated_reach=100 as a conservative default for subreddit broadcasts (actual subscriber count not available at this layer). causation_id = action_id (the action caused the reach event).
        // Transition the experiment assignment execution_status from
        // dispatched → executed. This is the actual execution boundary:
        // the external intervention (Reddit post) has been confirmed.
        // Monotonic: only dispatched → executed is allowed; if the
        // assignment is not in 'dispatched' state, this is a no-op.
        // Propagate trace_id from the autopilot action for trace continuity.
        sqlx::query(
            r#"
            UPDATE experiment_assignments
            SET execution_status = 'executed',
                trace_id = COALESCE(trace_id, (SELECT trace_id FROM autopilot_actions WHERE id = $2))
            WHERE workspace_id = $1
              AND action_id = $2
              AND execution_status = 'dispatched'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(action.action_id)
        .execute(&mut *posted_tx)
        .await?;
        posted_tx.commit().await?;
        tracing::info!(
            subreddit = %action.subreddit,
            post_url = %reddit_result.post_url,
            "successfully posted to Reddit"
        );
        Ok(())
    }

    /// Submits through the agents service (POST /reddit/post). The agents
    /// side owns the format ladder: `image_url` asks for a native image post
    /// (it downloads the bytes and runs Reddit's media-lease upload);
    /// `link_url` is what a link post points at — the draft's tracked
    /// `/l/` link when it has one, the source permalink otherwise; with
    /// neither working a plain self post goes out. The response's `kind`
    /// records which rung actually posted. A 429 maps to the executor's
    /// rate-limit backoff; every other failure is surfaced to the caller.
    async fn submit_via_agent_browser(
        &self,
        action: &ClaimedAction,
        post_body: &str,
        image_url: Option<&str>,
        link_url: Option<&str>,
    ) -> Result<RedditSubmitResult, CommunityExecutorError> {
        let auth_key = self.agent_service_auth_key.as_deref().ok_or_else(|| {
            CommunityExecutorError::RedditApi("agent service auth key not configured".to_owned())
        })?;
        let ws = self.workspace_id.into_uuid();
        let token = crate::discovery::derive_agent_token_with_capability(
            auth_key,
            ws,
            crate::discovery::AgentCapability::SocialPublish,
        );
        let url = format!("{}/reddit/post", self.agent_service_url);
        // `image_url` and `link_url` are zod `.optional()` on the agents side:
        // absent is a rung the ladder skips, but an explicit `null` fails
        // validation ("Expected string, received null") before the post is
        // even looked at — the keys only go on the wire when they carry a URL.
        let mut payload = serde_json::json!({
            "subreddit": action.subreddit,
            "title": action.title,
            "body": post_body,
        });
        if let Some(object) = payload.as_object_mut() {
            if let Some(url) = image_url {
                object.insert("image_url".to_owned(), url.into());
            }
            if let Some(url) = link_url {
                object.insert("link_url".to_owned(), url.into());
            }
        }

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
            .header(
                "X-Causation-Id",
                action
                    .causation_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
            )
            .header("X-Action-Id", action.action_id.to_string())
            .header(
                "X-Decision-Id",
                action
                    .decision_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
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
            if status.is_server_error() {
                // The agents service uses 5xx for everything that failed
                // before Reddit saw the post: no stored credentials (503),
                // a login that never produced a session (502), a crashed
                // browser (503). `RedditApi` here marked the draft failed —
                // one dead session burned every queued draft in a cycle and
                // told the brain the content had been refused.
                return Err(CommunityExecutorError::SessionUnavailable(format!(
                    "agents /reddit/post HTTP {status}: {body}"
                )));
            }
            return Err(CommunityExecutorError::RedditApi(format!(
                "agents /reddit/post HTTP {status}: {body}"
            )));
        }
        check_response_size(&response)?;
        response.json().await.map_err(CommunityExecutorError::Http)
    }

    /// Checks if this subreddit has been posted to within the cooldown window.
    async fn subreddit_on_cooldown(&self, subreddit: &str) -> Result<bool, CommunityExecutorError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM community_posts
            WHERE workspace_id = $1
              AND normalize_subreddit(subreddit) = normalize_subreddit($2)
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

    /// Builds the final post body, appending the smart link as a full URL
    /// if present. The smart_link stored in the action payload is a `/l/{slug}`
    /// path; Reddit needs a full URL for it to be clickable.
    ///
    /// An absolute URL is only appended when it points at this workspace's own
    /// origin. The field is written by a drafting model, and the two drafts
    /// production has produced so far carry `https://virya.com` and
    /// `https://virya.com/smartlink` — the band is at `virya.music`, so both
    /// are somebody else's domain, invented. Passing one through would send
    /// fans to a stranger's site and put an unrelated outbound link in a
    /// promotional post, which is the shape Reddit reads as spam. The account
    /// carrying that risk is the only one this channel has.
    ///
    /// A link that fails the check is dropped rather than corrected: the post
    /// still reads, and a guessed destination is not better than none. The
    /// resolution itself lives in `tracked_link_url`, which the link-post
    /// rung also uses for `link_url`.
    fn build_post_body<'a>(&'a self, body: &'a str, smart_link: Option<&str>) -> Cow<'a, str> {
        match self.tracked_link_url(smart_link) {
            Some(full_url) => Cow::Owned(format!("{body}\n\n{full_url}")),
            None => Cow::Borrowed(body),
        }
    }

    /// Resolves a stored `smart_link` to the absolute URL a platform needs —
    /// a `/l/...` path joins the workspace's public origin; an absolute URL
    /// only passes when it already points at that origin. Anything else is
    /// dropped rather than corrected: the post still reads, and a guessed
    /// destination is not better than none.
    fn tracked_link_url(&self, smart_link: Option<&str>) -> Option<String> {
        let link = smart_link?.trim();
        if link.is_empty() {
            return None;
        }
        if link
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("http"))
        {
            if self.is_own_origin(link) {
                return Some(link.to_owned());
            }
            tracing::warn!(
                link,
                origin = %self.public_origin,
                "dropping a smart link that points outside this workspace's origin"
            );
            return None;
        }
        if let Some(path) = link.strip_prefix('/') {
            // A path is the shape this field is supposed to carry, and
            // it can only ever resolve to our own origin.
            return Some(format!(
                "{}/{path}",
                self.public_origin.trim_end_matches('/')
            ));
        }
        tracing::warn!(
            link,
            "dropping a smart link that is neither an absolute URL nor a rooted path"
        );
        None
    }

    /// Whether an absolute URL belongs to this workspace's public origin.
    ///
    /// Compared on the origin, not with `starts_with`: `https://virya.music`
    /// is a prefix of `https://virya.music.evil.example`, and a prefix test
    /// would admit exactly the impersonation this guard exists to refuse.
    fn is_own_origin(&self, link: &str) -> bool {
        let origin = |url: &str| -> Option<String> {
            let rest = url.split_once("://")?.1;
            let host = rest.split(['/', '?', '#']).next()?;
            Some(host.trim_end_matches('.').to_ascii_lowercase())
        };
        match (origin(link), origin(&self.public_origin)) {
            (Some(link_host), Some(own_host)) => link_host == own_host,
            _ => false,
        }
    }

    /// The origins a link in a community post may point at: the workspace's
    /// own origin (smart links) and the owned-social domains a repost's
    /// permalink lives on. Trailing slashes keep `instagram.com.evil.example`
    /// from passing as `instagram.com`.
    fn community_approved_origins(&self) -> [String; 7] {
        [
            format!("{}/", self.public_origin.trim_end_matches('/')),
            "https://instagram.com/".to_owned(),
            "https://www.instagram.com/".to_owned(),
            "https://facebook.com/".to_owned(),
            "https://www.facebook.com/".to_owned(),
            "https://fb.watch/".to_owned(),
            "https://fb.com/".to_owned(),
        ]
    }

    /// Content hashes of what this channel posted recently — the dedupe input
    /// for the publish guard, so a relayed post that ships twice reads as the
    /// repeat it is.
    async fn recent_posted_hashes(&self) -> Result<BTreeSet<String>, CommunityExecutorError> {
        let bodies: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT body FROM community_posts
            WHERE workspace_id = $1
              AND status = 'posted'
              AND posted_at > now() - INTERVAL '7 days'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(bodies.iter().map(|b| content_hash(b)).collect())
    }

    /// Parks the draft for a person: `awaiting_manual_post` with the reason
    /// as the error message — the same hold shape `social_post_executor`
    /// writes when its publish guard refuses.
    async fn hold_for_human(
        &self,
        post_id: Uuid,
        reason: &str,
    ) -> Result<(), CommunityExecutorError> {
        sqlx::query(
            r#"
            UPDATE community_posts
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

    /// The still image to attach, fresh if re-minting is possible.
    ///
    /// Stored media URLs are signed CDN links and expire; when the action
    /// carries the Graph id and the workspace has a Page token, a fresh URL
    /// is minted at post time. IG media objects answer `media_url` /
    /// `thumbnail_url` (the video's still — the mp4 itself cannot be an
    /// image post); FB post objects answer `full_picture`. The two field
    /// sets are tried in order because the id alone does not say which
    /// object kind it names.
    async fn resolve_image_url(&self, action: &ClaimedAction) -> Option<String> {
        let (Some(media_id), Some(token)) = (
            action.media_id.as_deref(),
            self.facebook_page_access_token.as_deref(),
        ) else {
            return action.image_url.clone();
        };
        for fields in ["media_url,thumbnail_url", "full_picture"] {
            let url = format!(
                "https://graph.facebook.com/v21.0/{media_id}?fields={fields}&access_token={token}"
            );
            let client = self
                .http_client
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let response = match client.get(&url).send().await {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(
                        media_id,
                        error = %error.without_url(),
                        "media url re-mint request failed — falling back to the stored url"
                    );
                    continue;
                }
            };
            if !response.status().is_success() {
                continue;
            }
            let parsed: serde_json::Value = match response.json().await {
                Ok(parsed) => parsed,
                Err(_) => continue,
            };
            // A video's `media_url` is the mp4 — the still is its thumbnail.
            // A photo has no thumbnail, so the order reads: still first,
            // then the media itself, then the FB shape.
            let fresh = parsed
                .get("thumbnail_url")
                .and_then(Value::as_str)
                .or_else(|| parsed.get("media_url").and_then(Value::as_str))
                .or_else(|| parsed.get("full_picture").and_then(Value::as_str));
            if let Some(fresh) = fresh {
                return Some(fresh.to_owned());
            }
        }
        action.image_url.clone()
    }

    /// Polls Reddit for post performance metrics on recently posted content.
    /// Only polls posts that:
    /// - Have `status = 'posted'` with a non-null `reddit_post_id`
    /// - Were posted within the last `METRICS_WINDOW` (72h)
    /// - Haven't been polled in the last `METRICS_POLL_MIN_INTERVAL` / `_SETTLED_INTERVAL`
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
                  OR metrics_last_fetched_at < now() - make_interval(secs => CASE
                      WHEN posted_at > now() - INTERVAL '6 hours' THEN $3::double precision
                      ELSE $5::double precision END)
              )
            ORDER BY posted_at DESC
            LIMIT $4
            "#,
        )
        .bind(ws)
        .bind(METRICS_WINDOW.as_secs() as i64)
        .bind(METRICS_POLL_MIN_INTERVAL.as_secs() as i64)
        .bind(METRICS_POLL_BATCH)
        .bind(METRICS_POLL_SETTLED_INTERVAL.as_secs() as i64)
        .fetch_all(&self.pool)
        .await?;

        if posts_to_poll.is_empty() {
            return Ok(0);
        }

        // Metrics are read through the agent service, falling back to
        // Reddit's public JSON.
        //
        // That fallback no longer reaches anything. Reddit requires
        // authentication for the JSON API now — `/comments/{id}.json` answers
        // 403 from a datacenter host and from a residential connection alike —
        // so the fallback exists for the day access is restored, not as a
        // route that works today. This comment used to claim metrics polling
        // "keeps the feedback loop working even when the Reddit API is
        // unavailable for posting", which is exactly backwards: without a
        // credential neither posting nor measuring works, and a manual post
        // registered by an operator still records no engagement.
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

    /// Reads Reddit session cookies directly from the database (obtained by
    /// the Playwright scraper via Google OAuth). Returns None if no active
    /// cookies are stored. Reads the `agent_service_reddit_cookies` table
    /// directly instead of calling the now-locked-down `/reddit/cookies`
    /// endpoint, which returns status-only metadata.
    async fn fetch_reddit_cookies(&self) -> Option<String> {
        crate::discovery::fetch_reddit_cookies_from_db(&self.pool, self.workspace_id.into_uuid())
            .await
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
        let token = crate::discovery::derive_agent_token_with_capability(
            auth_key,
            ws,
            crate::discovery::AgentCapability::Read,
        );
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
        check_response_size(&response)?;
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
        check_response_size(&response)?;

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
            removed_by_category: data
                .data
                .removed_by_category
                .as_ref()
                .and_then(Value::as_str)
                .map(str::to_owned),
            removal_visible: data.data.removed_by_category.is_some(),
        })
    }

    /// Updates only the `metrics_last_fetched_at` timestamp, used when a
    /// metrics fetch fails so we don't retry the same post immediately.
    async fn touch_metrics_fetched_at(&self, post_id: Uuid) -> Result<(), CommunityExecutorError> {
        sqlx::query(
            r#"
            UPDATE community_posts
            SET metrics_last_fetched_at = now(),
                updated_at = now()
            WHERE id = $1 AND workspace_id = $2
            "#,
        )
        .bind(post_id)
        .bind(self.workspace_id.into_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
pub struct ClaimedAction {
    id: Uuid,
    /// The status this row held before the claim.
    ///
    /// `awaiting_manual_post` means the draft was written for a person to
    /// publish and is now being adopted by the executor. That is the one claim
    /// worth announcing: if the operator already published it by hand and never
    /// registered the URL, nothing in this system can know, and posting it again
    /// would be a second post under the band's name. Reported before the
    /// attempt, so it is in the log beside the post rather than after it.
    claimed_from: String,
    action_id: Uuid,
    /// The community this delivery targets — the outreach target id a
    /// standing grant keys on. `None` means no grant can cover the row.
    target_id: Option<Uuid>,
    subreddit: String,
    title: String,
    body: String,
    smart_link: Option<String>,
    /// The reposted post's own image, when the source had one. A signed CDN
    /// URL that may have expired since the sync stored it — `media_id`
    /// re-mints a fresh one at post time.
    image_url: Option<String>,
    /// Graph object id the image belongs to — `/{id}?fields=media_url` (or
    /// `thumbnail_url`, or `full_picture` for a FB post) re-mints the URL.
    media_id: Option<String>,
    /// The band's own permalink — the link-post fallback when no image can
    /// be carried, and the post's attribution target.
    source_url: Option<String>,
    /// The relay batch this delivery belongs to. `Some` means the row posts
    /// under the batch's own interval — the workspace 24h cap does not
    /// govern it.
    relay_source_id: Option<Uuid>,
    trace_id: Option<Uuid>,
    causation_id: Option<Uuid>,
    decision_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct RedditSubmitResult {
    post_id: String,
    post_url: String,
    /// What actually shipped — `image` | `link` | `self`. The agents service
    /// reports it so the ledger records the post that exists, not the one
    /// that was attempted.
    #[serde(default)]
    kind: Option<String>,
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
    /// Absent: the listing said nothing about removal. `Some(Null)`: it
    /// said the post is live. Kept apart so a missing key never reads as live.
    #[serde(default, deserialize_with = "standing::present_value")]
    removed_by_category: Option<Value>,
}

/// Parsed metrics from a Reddit post, ready to record.
#[derive(Deserialize)]
struct RedditPostMetrics {
    score: i32,
    upvotes: i32,
    num_comments: i32,
    upvote_ratio: Option<f64>,
    /// Reddit's `removed_by_category`, when the read could see it.
    #[serde(default)]
    removed_by_category: Option<String>,
    /// Whether the read could establish removal state at all. An agents
    /// service that predates the field omits it, and its reads prove nothing.
    #[serde(default)]
    removal_visible: bool,
}

include!("community_executor/tests.rs");
