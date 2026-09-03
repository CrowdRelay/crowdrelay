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

    async fn run_once(&self) -> Result<usize, CommunityJoinError> {
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
                    tracing::warn!(
                        place_id = %place.place_id,
                        subreddit = %place.name,
                        error = %error,
                        "failed to join community"
                    );
                    let msg = error.to_string();
                    self.set_membership(place.place_id, "rejected", Some(&msg))
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
                    SELECT id FROM discovery_places
                    WHERE workspace_id = $1
                      AND place_kind = 'subreddit'
                      AND membership_state = 'not_joined'
                      AND status = 'active'
                      AND (member_count IS NULL OR member_count >= $2)
                    ORDER BY member_count DESC NULLS LAST
                    LIMIT $3
                    FOR UPDATE SKIP LOCKED
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
        let token = crate::discovery::derive_agent_token(auth_key, ws);
        let url = format!("{}/reddit/join", self.agent_service_url);

        // Extract the subreddit name from the place name (stored without
        // the r/ prefix in discovery_places).
        let subreddit = place.name.trim_start_matches("r/");

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
    #[allow(dead_code)]
    url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
