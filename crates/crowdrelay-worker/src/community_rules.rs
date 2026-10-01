//! Community rules refresh — the periodic read that keeps
//! `discovery_place_rules` honest.
//!
//! The screening policy refuses a community whose measured self-promo ratio
//! is zero, and the seed gate parks a draft whose community requires a flair
//! the executor cannot set. Both read this table. For as long as nothing
//! wrote it, both were dead code — promotion-hostile subreddits screened
//! `admitted` on vibes alone, and two of them removed the account's posts in
//! the same week, which is exactly the evidence this table exists to see
//! *before* it happens.
//!
//! One pass takes the subreddits whose rules are missing or stale (30 days),
//! asks the agents service for `about` + `about/rules` through the same
//! authenticated session the observation adapter borrows, classifies the
//! rule texts with the domain's conservative classifier, and writes the
//! measured stance back. When the stance comes back `Banned`, a target that
//! is still admitted is refused in the same transaction — the rules are the
//! community's own words, and a community that bans what we post is a wrong
//! target no matter how good its audience looked.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::community_rules::{SelfPromoStance, classify_self_promo, summarize};
use tokio::sync::watch;
use tracing::{info, warn};

/// Rules change rarely; a month is fresh enough for a document a moderator
/// edits maybe twice a year. `verified_at` carries the measurement date, so
/// a rules row that stopped refreshing is visibly stale rather than silent.
const RULES_STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How often the sweep wakes. Small batch, frequent wake: a shared-IP rate
/// limit mid-batch (Reddit's 429 on the cookie session) must not stretch the
/// refresh out to days.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Places refreshed per pass. The fetch rides the single authenticated
/// browser/cookie session — bursting all 94 subreddits in one pass earned a
/// 46-request 429 wall on the shared IP. Paced batches are how the whole
/// registry actually gets measured.
const BATCH_LIMIT: i64 = 10;

/// Pause between place fetches inside one pass.
const REQUEST_PACE: Duration = Duration::from_secs(2);

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// A subreddit place whose rules need (re)measuring.
#[derive(sqlx::FromRow)]
struct StalePlace {
    id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    url: String,
}

/// The agents service's answer for `POST /reddit/community-info`.
#[derive(serde::Deserialize)]
struct CommunityInfoResponse {
    #[serde(default)]
    rules: Vec<CommunityRule>,
}

#[derive(serde::Deserialize)]
struct CommunityRule {
    #[serde(default)]
    short_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

pub struct CommunityRulesWorker {
    pool: sqlx::PgPool,
    workspace_id: uuid::Uuid,
    agent_service_url: String,
    agent_service_auth_key: String,
    http: reqwest::Client,
}

impl CommunityRulesWorker {
    /// `None` without an agents-service auth key, matching the adapters'
    /// own logic: a sweep that would 401 every request is worse than one
    /// that never registers.
    pub fn new(
        pool: sqlx::PgPool,
        workspace_id: WorkspaceId,
        agent_service_url: String,
        agent_service_auth_key: Option<String>,
    ) -> Option<Self> {
        let key = agent_service_auth_key
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())?;
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent("crowdrelay-community-rules/1")
            .build()
            .ok()?;
        Some(Self {
            pool,
            workspace_id: workspace_id.into_uuid(),
            agent_service_url: agent_service_url.trim_end_matches('/').to_owned(),
            agent_service_auth_key: key,
            http,
        })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        info!("community rules refresh worker starting");
        let mut tick = tokio::time::interval(Duration::from_secs(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut first = true;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if first {
                        // Due immediately on boot — the backlog is the point —
                        // then on the regular interval.
                        first = false;
                        tick = tokio::time::interval(SWEEP_INTERVAL);
                        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    }
                    if let Err(e) = self.refresh_once().await {
                        warn!(error = %e, "community rules sweep failed");
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("community rules refresh worker shutting down");
                        break;
                    }
                }
            }
        }
    }

    /// One paced pass over the stalest unmeasured places.
    pub async fn refresh_once(&self) -> Result<usize, sqlx::Error> {
        let places = sqlx::query_as::<_, StalePlace>(
            r#"
            SELECT p.id, p.workspace_id, p.url
            FROM discovery_places p
            LEFT JOIN discovery_place_rules r ON r.place_id = p.id
            WHERE p.workspace_id = $1
              AND p.place_kind = 'subreddit'
              AND p.status = 'active'
              AND (r.verified_at IS NULL
                   OR r.verified_at < now() - make_interval(secs => $2))
            ORDER BY r.verified_at NULLS FIRST, p.id
            LIMIT $3
            "#,
        )
        .bind(self.workspace_id)
        .bind(RULES_STALE_AFTER.as_secs() as i64)
        .bind(BATCH_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let mut refreshed = 0;
        for place in &places {
            let Some(subreddit) = subreddit_from_url(&place.url) else {
                warn!(place_id = %place.id, url = %place.url, "no subreddit in place url — skipped");
                continue;
            };
            match self.fetch_community_info(&subreddit).await {
                Ok(info) => {
                    if let Err(e) = self.store_rules(place, &info).await {
                        warn!(place_id = %place.id, error = %e, "failed to store community rules");
                    } else {
                        refreshed += 1;
                    }
                }
                Err(FetchError::NotFound) => {
                    // The subreddit is gone or private — a measurement of its
                    // own kind. `archived` keeps it out of join lanes and
                    // candidate lists without calling it "ruled out": the
                    // community died, we did not refuse it.
                    let _ = sqlx::query(
                        "UPDATE discovery_places SET status = 'archived', updated_at = now() \
                         WHERE id = $1 AND workspace_id = $2",
                    )
                    .bind(place.id)
                    .bind(place.workspace_id)
                    .execute(&self.pool)
                    .await;
                }
                Err(e) => {
                    warn!(place_id = %place.id, subreddit = %subreddit, error = %e,
                          "community-info fetch failed");
                }
            }
            tokio::time::sleep(REQUEST_PACE).await;
        }
        if refreshed > 0 {
            info!(refreshed, "community rules refreshed");
        }
        Ok(refreshed)
    }

    async fn fetch_community_info(
        &self,
        subreddit: &str,
    ) -> Result<CommunityInfoResponse, FetchError> {
        let token = crate::discovery::derive_agent_token_with_capability(
            &self.agent_service_auth_key,
            self.workspace_id,
            crate::discovery::AgentCapability::Read,
        );
        let response = self
            .http
            .post(format!("{}/reddit/community-info", self.agent_service_url))
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Workspace-Id", self.workspace_id.to_string())
            .json(&serde_json::json!({ "subreddit": subreddit }))
            .send()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Err(FetchError::NotFound);
        }
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        response
            .json()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))
    }

    /// Classifies and persists the measured stance, and — inside the same
    /// transaction — applies it: a community whose own rules ban what we
    /// post refuses its admitted target, so the engager stops drafting it
    /// the same sweep the measurement lands.
    async fn store_rules(
        &self,
        place: &StalePlace,
        info: &CommunityInfoResponse,
    ) -> Result<(), sqlx::Error> {
        let texts: Vec<String> = info
            .rules
            .iter()
            .flat_map(|r| {
                [
                    r.short_name.as_deref().unwrap_or(""),
                    r.description.as_deref().unwrap_or(""),
                ]
            })
            .map(str::to_owned)
            .collect();
        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let stance = classify_self_promo(&text_refs);
        let (ratio, requires_approval) = stance.columns();

        // The classifier already reads descriptions; keep the same moderator
        // words in rules_summary so the drafter and operator can obey the
        // actual constraint instead of seeing only a vague heading.
        let rule_summaries: Vec<String> = info
            .rules
            .iter()
            .map(|r| {
                let title = r.short_name.as_deref().unwrap_or("").trim();
                let description = r.description.as_deref().unwrap_or("").trim();
                match (title.is_empty(), description.is_empty()) {
                    (false, false) => format!("{title} :: {description}"),
                    (false, true) => title.to_owned(),
                    (true, false) => description.to_owned(),
                    (true, true) => String::new(),
                }
            })
            .collect();
        let summary_refs: Vec<&str> = rule_summaries.iter().map(String::as_str).collect();
        let summary = summarize(&summary_refs);

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO discovery_place_rules
                (place_id, self_promo_ratio_percent, requires_approval,
                 rules_summary, verified_at, updated_at)
            VALUES ($1, $2, $3, $4, now(), now())
            ON CONFLICT (place_id) DO UPDATE SET
                self_promo_ratio_percent = EXCLUDED.self_promo_ratio_percent,
                requires_approval = EXCLUDED.requires_approval,
                rules_summary = EXCLUDED.rules_summary,
                verified_at = now(),
                updated_at = now()
            "#,
        )
        .bind(place.id)
        .bind(ratio)
        .bind(requires_approval)
        .bind(summary)
        .execute(&mut *tx)
        .await?;

        if matches!(stance, SelfPromoStance::Banned) {
            // The community's own rules, not a heuristic about it: a target
            // that survives this has a rule that explicitly bans, confines
            // or participation-gates what we would post.
            sqlx::query(
                r#"
                UPDATE agent_outreach_targets
                SET screening_verdict = 'refused',
                    refusal_reason = 'poor_fit',
                    status = 'discarded',
                    updated_at = now()
                WHERE workspace_id = $1
                  AND place_id = $2
                  AND target_kind = 'community'
                  AND screening_verdict = 'admitted'
                "#,
            )
            .bind(place.workspace_id)
            .bind(place.id)
            .execute(&mut *tx)
            .await?;
            // Any draft already seeded for that target dies with it — same
            // wall the executor's claim-time veto builds, landed early.
            sqlx::query(
                r#"
                UPDATE community_posts
                SET status = 'cancelled',
                    error_message = 'cancelled: community rules ban self-promotion',
                    updated_at = now()
                WHERE workspace_id = $1
                  AND place_id = $2
                  AND status IN ('pending', 'rate_limited', 'awaiting_manual_post')
                "#,
            )
            .bind(place.workspace_id)
            .bind(place.id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// `r/{name}` out of a stored place URL — same extraction the Reddit adapter
/// uses, kept local so this worker has no adapter dependency.
fn subreddit_from_url(url: &str) -> Option<String> {
    let after = url.split("/r/").nth(1)?;
    let name = after.split(['/', '?', '#']).next()?.trim();
    if name.is_empty() || name.len() > 21 {
        return None;
    }
    Some(name.to_owned())
}

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("not found")]
    NotFound,
}
