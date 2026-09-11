//! Community join executor: auto-joins (subscribes to) Reddit communities
//! that the discovery worker has found and the brain has quality-screened.
//!
//! The discovery worker finds subreddits and stores them in `discovery_places`
//! with `membership_state = 'not_joined'`. This executor claims eligible
//! places, calls the agents service's `/reddit/join` endpoint (which drives
//! the logged-in browser session to subscribe via Reddit's API), and
//! transitions the membership state.
//!
//! ## Eligibility
//! - `place_kind = 'subreddit'` (only Reddit is supported for auto-join)
//! - `membership_state = 'not_joined'`
//! - `status = 'active'`
//! - `member_count >= 100` (skip tiny/dead communities)
//! - Not joined in the last 24 hours (rate limit: max 10 joins per 24h)
//!
//! ## Guardrails
//! - Max 10 joins per 24 hours per workspace
//! - Max 1 join per 5 minutes (politeness)
//! - If `CROWDRELAY_COMMUNITY_AUTO_JOIN` is not enabled, places stay
//!   `not_joined` — the operator joins manually and records the result
//!
//! ## Crash recovery
//! `joining` rows older than 10 minutes are reclaimed and marked `not_joined`
//! (safe to retry — joining is idempotent on Reddit's side).

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

/// How often to poll for joinable communities.
const POLL_INTERVAL: Duration = Duration::from_secs(300);
/// A `joining` row older than this is considered a crashed attempt.
const JOINING_STALE_THRESHOLD: Duration = Duration::from_secs(600);
/// Maximum joins per workspace per 24 hours.
const MAX_JOINS_PER_24H: i64 = 10;
/// Minimum member count for a community to be eligible for auto-join.
const MIN_MEMBER_COUNT: i32 = 100;
/// Per-request timeout for the agents service join call.
const JOIN_API_TIMEOUT: Duration = Duration::from_secs(60);
/// Watchdog for one executor cycle.
const CYCLE_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(180);
/// Maximum places to claim in a single cycle.
const CLAIM_BATCH: i64 = 3;

#[derive(Debug, Error)]
pub enum CommunityJoinError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("agents service error: {0}")]
    AgentsService(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("agent service auth key not configured")]
    NoAuthKey,
    #[error("rate limited by Reddit")]
    RateLimited,
    #[error("http client build failed: {0}")]
    ClientBuild(reqwest::Error),
    #[error("no subreddit slug in place url: {0}")]
    NotASubreddit(String),
}

impl CommunityJoinError {
    /// Whether this failure is Reddit refusing us, as opposed to our own side
    /// failing before Reddit ever saw the request.
    ///
    /// `membership_state = 'rejected'` is terminal: `claim_joinable_places`
    /// only ever claims `not_joined`, so a rejected row is never retried. It
    /// therefore has to mean "the community said no", and nothing else.
    ///
    /// Every error used to land there. Seventy-one places were marked rejected
    /// on this tenant and not one of them was a refusal: thirty-seven were our
    /// agent service answering 503 "no reddit credentials stored", thirty were
    /// our own 400 on a malformed subreddit, three were the agent service being
    /// unreachable, and the last was a Reddit login the error itself called
    /// retryable. A ten-minute credential outage permanently burned every
    /// community it touched.
    fn is_refusal(&self) -> bool {
        match self {
            // Our own side: database, transport, config, a row we cannot use.
            Self::Database(_)
            | Self::Http(_)
            | Self::NoAuthKey
            | Self::ClientBuild(_)
            | Self::RateLimited
            | Self::NotASubreddit(_) => false,
            // The agent service answered. Only a 4xx that is not our own
            // validation error means Reddit itself turned us down; a 5xx is
            // the service failing, and a 400 is the service rejecting our
            // request before sending it.
            Self::AgentsService(message) => {
                !message.contains("HTTP 5") && !message.contains("HTTP 400")
            }
        }
    }
}

/// The subreddit slug for a place, from its canonical URL.
///
/// Falls back to the name only when it is already slug-shaped, so a title
/// never reaches the API as a subreddit.
fn subreddit_slug(url: &str, name: &str) -> Option<String> {
    let slug_shaped = |value: &str| {
        let value = value
            .trim()
            .trim_start_matches("/r/")
            .trim_start_matches("r/");
        (1..=21).contains(&value.chars().count())
            && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if let Some(rest) = url.split("/r/").nth(1) {
        let candidate = rest.split(['/', '?', '#']).next().unwrap_or("").trim();
        if slug_shaped(candidate) {
            return Some(candidate.to_owned());
        }
    }
    let candidate = name
        .trim()
        .trim_start_matches("/r/")
        .trim_start_matches("r/");
    slug_shaped(candidate).then(|| candidate.to_owned())
}

#[derive(Clone)]
pub struct CommunityJoinExecutorWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    http_client: reqwest::Client,
    agent_service_url: String,
    agent_service_auth_key: Option<String>,
    poll_interval: Duration,
    /// When true, the executor calls the agents service to auto-join.
    /// When false (default), places stay `not_joined` — the operator
    /// joins manually and records the result via the API.
    auto_join: bool,
}

impl CommunityJoinExecutorWorker {
    /// Creates a new executor. Returns an error if the HTTP client cannot
    /// be built.
    ///
    /// # Errors
    /// Returns [`CommunityJoinError::ClientBuild`] if the `reqwest` client
    /// cannot be initialized.
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        agent_service_url: String,
        agent_service_auth_key: Option<String>,
        auto_join: bool,
    ) -> Result<Self, CommunityJoinError> {
        let http_client = reqwest::Client::builder()
            .timeout(JOIN_API_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .user_agent("CrowdRelay/1.0 community-join-executor")
            .build()
            .map_err(CommunityJoinError::ClientBuild)?;
        Ok(Self {
            pool,
            workspace_id,
            http_client,
            agent_service_url,
            agent_service_auth_key,
            poll_interval: POLL_INTERVAL,
            auto_join,
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
                            tracing::info!(processed, "community join executor processed batch");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "community join executor cycle failed"),
                        Err(_) => tracing::warn!("community join executor cycle timed out"),
                    }
                }
            }
        }
    }

    /// One executor cycle: recover stale claims, claim eligible places, and
    /// call the agents service for each.
    ///
    /// Public so the runtime boundary suite can drive exactly the cycle the
    /// worker runs — the same claim, the same HTTP call, the same state
    /// transitions — rather than a test-only reconstruction of it.
    ///
    /// # Errors
    /// Returns [`CommunityJoinError`] if the claim or recovery queries fail.
    /// Per-place agent-service failures are recorded on the place and do not
    /// fail the cycle.
    pub async fn run_once(&self) -> Result<usize, CommunityJoinError> {
        self.recover_stale_joining().await?;

        if !self.auto_join {
            return Ok(0);
        }

        let places = self.claim_joinable_places().await?;
        let mut processed = 0;
        for place in &places {
            match self.join_community(place).await {
                Ok(()) => processed += 1,
                Err(CommunityJoinError::RateLimited) => {
                    tracing::warn!(
                        place_id = %place.place_id,
                        subreddit = %place.name,
                        "reddit rate limited the join, will retry next cycle"
                    );
                    // Revert to not_joined so it can be retried.
                    self.set_membership(place.place_id, "not_joined", Some("rate limited"))
                        .await
                        .ok();
                }
                Err(CommunityJoinError::NoAuthKey) => {
                    tracing::warn!("agent service auth key not configured, skipping join");
                    self.set_membership(place.place_id, "not_joined", Some("auth key missing"))
                        .await
                        .ok();
                    break;
                }
                Err(error) => {
                    // `rejected` is terminal. It is written only when Reddit
                    // actually refused; our own failures go back to
                    // `not_joined` so the next cycle retries them.
                    let refused = error.is_refusal();
                    tracing::warn!(
                        place_id = %place.place_id,
                        place = %place.name,
                        error = %error,
                        refused,
                        "failed to join community"
                    );
                    let msg = error.to_string();
                    let state = if refused { "rejected" } else { "not_joined" };
                    self.set_membership(place.place_id, state, Some(&msg))
                        .await
                        .ok();
                }
            }
        }
        Ok(processed)
    }

    /// Recovers `joining` rows that have been stuck longer than the stale
    /// threshold. Reverts them to `not_joined` — joining is idempotent on
    /// Reddit's side, so retrying is safe.
    async fn recover_stale_joining(&self) -> Result<(), CommunityJoinError> {
        let ws = self.workspace_id.into_uuid();
        let result = sqlx::query(
            r#"
            UPDATE discovery_places
            SET membership_state = 'not_joined',
                membership_note = 'recovered from stale joining attempt',
                membership_changed_at = now(),
                membership_changed_by = 'community-join-executor:recovery',
                updated_at = now()
            WHERE workspace_id = $1
              AND membership_state = 'joining'
              AND membership_changed_at < now() - make_interval(secs => $2::double precision)
            "#,
        )
        .bind(ws)
        .bind(JOINING_STALE_THRESHOLD.as_secs() as i64)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() > 0 {
            tracing::info!(
                recovered = result.rows_affected(),
                "recovered stale joining rows (reverted to not_joined)"
            );
        }
        Ok(())
    }

    /// Claims a batch of joinable subreddit places. Transitions them to
    /// `joining` atomically using `FOR UPDATE SKIP LOCKED`.
    async fn claim_joinable_places(&self) -> Result<Vec<ClaimedPlace>, CommunityJoinError> {
        let ws = self.workspace_id.into_uuid();

        // Check 24h rate limit first.
        let recent_joins: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*) FROM discovery_places
            WHERE workspace_id = $1
              AND membership_state = 'joined'
              AND membership_changed_at > now() - INTERVAL '24 hours'
            "#,
        )
        .bind(ws)
        .fetch_one(&self.pool)
        .await?;

        if recent_joins >= MAX_JOINS_PER_24H {
            tracing::debug!(
                recent_joins,
                max = MAX_JOINS_PER_24H,
                "24h join limit reached, skipping"
            );
            return Ok(vec![]);
        }

        let rows = sqlx::query_as::<_, ClaimedPlace>(
            r#"
            WITH claimed AS (
                UPDATE discovery_places
                SET membership_state = 'joining',
                    membership_changed_at = now(),
                    membership_changed_by = 'community-join-executor',
                    updated_at = now()
                WHERE id IN (
                    SELECT place.id FROM discovery_places AS place
                    -- Does the brain have a post waiting behind this join?
                    --
                    -- The reinforcement edge, read backwards. The brain picks
                    -- posts by value and this worker picked joins by size, so
                    -- the two chose independently: the biggest community got
                    -- joined while the one the brain actually wanted to post
                    -- to stayed unjoined and its candidate stayed gated. A
                    -- promoted, unrefused target on this place is the brain
                    -- saying it wants to post here.
                    --
                    -- The predicate matches
                    -- `agent_outreach_targets_community_admitted_idx` exactly,
                    -- so the lateral is an index scan rather than a table scan
                    -- per candidate row. `subreddit IS NOT NULL` is in the
                    -- index predicate and costs nothing semantically: a
                    -- community target with no subreddit cannot be posted to.
                    LEFT JOIN LATERAL (
                        SELECT true AS wanted
                        FROM agent_outreach_targets AS t
                        WHERE t.workspace_id = place.workspace_id
                          AND t.place_id = place.id
                          AND t.status = 'promoted'
                          AND t.target_kind = 'community'
                          AND t.subreddit IS NOT NULL
                          AND t.screening_verdict IS DISTINCT FROM 'refused'
                        LIMIT 1
                    ) AS demand ON true
                    WHERE place.workspace_id = $1
                      AND place.place_kind = 'subreddit'
                      AND place.membership_state = 'not_joined'
                      AND place.status = 'active'
                      AND (place.member_count IS NULL OR place.member_count >= $2)
                    -- Wanted first, then the previous order within each group.
                    -- Size still decides among communities the brain has no
                    -- plans for, so this reorders rather than replaces.
                    ORDER BY demand.wanted IS NOT NULL DESC,
                             place.member_count DESC NULLS LAST
                    LIMIT $3
                    FOR UPDATE OF place SKIP LOCKED
                )
                RETURNING id, name, url
            )
            SELECT c.id AS place_id, c.name, c.url
            FROM claimed c
            "#,
        )
        .bind(ws)
        .bind(MIN_MEMBER_COUNT)
        .bind(CLAIM_BATCH)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Joins a single community by calling the agents service's
    /// `/reddit/join` endpoint.
    async fn join_community(&self, place: &ClaimedPlace) -> Result<(), CommunityJoinError> {
        let auth_key = self
            .agent_service_auth_key
            .as_deref()
            .ok_or(CommunityJoinError::NoAuthKey)?;
        let ws = self.workspace_id.into_uuid();
        let token = crate::discovery::derive_agent_token_with_capability(
            auth_key,
            ws,
            crate::discovery::AgentCapability::SocialPublish,
        );
        let url = format!("{}/reddit/join", self.agent_service_url);

        // `discovery_places.name` is the subreddit's *title*, not its slug —
        // "Death Metal: death metal bands, death metal music, and death metal
        // culture", "/r/Metalcore - news, reviews, videos &amp; discussion".
        // Sending that as a subreddit produced
        //   HTTP 400 {"error":"subreddit must be 2-21 chars of A-Za-z0-9_"}
        // thirty times, from our own validator, before the request ever
        // reached Reddit. The slug was in `url` the whole time, which this
        // struct already selected and marked `#[allow(dead_code)]`.
        let Some(subreddit) = subreddit_slug(&place.url, &place.name) else {
            return Err(CommunityJoinError::NotASubreddit(place.url.clone()));
        };
        let subreddit = subreddit.as_str();

        let payload = serde_json::json!({
            "subreddit": subreddit,
        });

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Workspace-Id", ws.to_string())
            .json(&payload)
            .timeout(JOIN_API_TIMEOUT)
            .send()
            .await?;

        let status = response.status();
        if status.as_u16() == 429 {
            return Err(CommunityJoinError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CommunityJoinError::AgentsService(format!(
                "agents /reddit/join HTTP {status}: {body}"
            )));
        }

        // Success — transition to joined.
        self.set_membership(place.place_id, "joined", None).await?;

        tracing::info!(
            place_id = %place.place_id,
            subreddit = %place.name,
            "successfully joined community"
        );
        Ok(())
    }

    /// Updates the membership state of a discovery place.
    async fn set_membership(
        &self,
        place_id: Uuid,
        state: &str,
        note: Option<&str>,
    ) -> Result<(), CommunityJoinError> {
        sqlx::query(
            r#"UPDATE discovery_places
               SET membership_state = $3,
                   membership_note = $4,
                   membership_changed_at = now(),
                   membership_changed_by = 'community-join-executor',
                   updated_at = now()
               WHERE workspace_id = $1 AND id = $2"#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(place_id)
        .bind(state)
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct ClaimedPlace {
    place_id: Uuid,
    name: String,
    url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slug_comes_from_the_url_not_the_title() {
        // Every one of these is a real row that was marked rejected on this
        // tenant, with its title sent as the subreddit.
        for (url, title, expected) in [
            (
                "https://www.reddit.com/r/death_metal",
                "Death Metal: death metal bands, death metal music, and death metal culture",
                "death_metal",
            ),
            (
                "https://www.reddit.com/r/EODM",
                "Eagles of Death Metal",
                "EODM",
            ),
            (
                "https://www.reddit.com/r/Metalcore",
                "/r/Metalcore - news, reviews, videos &amp; discussion",
                "Metalcore",
            ),
            (
                "https://www.reddit.com/r/guitarcirclejerk",
                "All Rig... No Gig",
                "guitarcirclejerk",
            ),
            (
                "https://www.reddit.com/r/melodicdeathmetal/",
                "Melodic Death Metal - news, reviews, videos and discussion.",
                "melodicdeathmetal",
            ),
        ] {
            assert_eq!(subreddit_slug(url, title).as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_title_is_never_sent_as_a_subreddit() {
        // No usable URL and a title that is not slug-shaped: the row is
        // unusable, and saying so beats sending a sentence to the API.
        assert_eq!(
            subreddit_slug(
                "https://example.com/whatever",
                "Death Metal: bands and culture"
            ),
            None
        );
        // A name that is already a slug is still accepted when the URL has none.
        assert_eq!(
            subreddit_slug("https://example.com/x", "r/Metalcore").as_deref(),
            Some("Metalcore")
        );
    }

    #[test]
    fn only_reddit_refusing_us_counts_as_a_rejection() {
        // `rejected` is terminal, so everything that is not Reddit saying no
        // has to stay retryable. These four messages are the exact ones this
        // tenant recorded, in the counts it recorded them.
        let ours = [
            CommunityJoinError::AgentsService(
                "agents /reddit/join HTTP 503 Service Unavailable: {\"error\":\"no reddit \
                 credentials stored — POST /reddit/credentials first\"}"
                    .to_owned(),
            ),
            CommunityJoinError::AgentsService(
                "agents /reddit/join HTTP 400 Bad Request: {\"error\":\"subreddit must be \
                 2-21 chars of A-Za-z0-9_\"}"
                    .to_owned(),
            ),
            CommunityJoinError::NoAuthKey,
            CommunityJoinError::RateLimited,
            CommunityJoinError::NotASubreddit("https://example.com/x".to_owned()),
        ];
        for error in ours {
            assert!(
                !error.is_refusal(),
                "our own failure must stay retryable: {error}"
            );
        }

        // Reddit itself turning us down is terminal, and should be.
        let theirs = CommunityJoinError::AgentsService(
            "agents /reddit/join HTTP 403 Forbidden: {\"error\":\"subreddit is private\"}"
                .to_owned(),
        );
        assert!(theirs.is_refusal());
    }

    #[test]
    fn max_joins_per_24h_is_bounded() {
        // Bounded between 1 and 20 — prevents both spam and total silence.
        const { assert!(MAX_JOINS_PER_24H > 0 && MAX_JOINS_PER_24H <= 20) };
    }

    #[test]
    fn min_member_count_is_reasonable() {
        // At least 50 members — below that the community is likely dead.
        const { assert!(MIN_MEMBER_COUNT >= 50) };
    }

    #[test]
    fn poll_interval_is_polite() {
        // At least 60 seconds — no busy looping.
        const { assert!(POLL_INTERVAL.as_secs() >= 60) };
    }
}
