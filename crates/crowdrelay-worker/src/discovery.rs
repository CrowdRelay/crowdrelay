//! Deterministic discovery sweeps for the Audience Graph.
//!
//! First adapter: public Reddit subreddit search. The worker is deliberately
//! dumb — it fetches, normalizes and upserts places with a raw evidence
//! payload; scoring, outreach decisions and anything resembling judgment stay
//! in the domain/autopilot layers. Disabled unless discovery queries are
//! configured, so existing deployments make zero network calls.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::audience_graph::{
    EvidenceInput, PostgresAudienceGraphRepository, UpsertPlaceInput,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, timeout},
};
use uuid::Uuid;

const USER_AGENT: &str = "CrowdRelay/1.0 audience-graph-discovery";
/// Reddit's unauthenticated JSON API tolerates roughly one request per second
/// per client; two seconds keeps the sweep polite without making it useless.
const REQUEST_SPACING: Duration = Duration::from_secs(2);
const MAX_SUBREDDITS_PER_QUERY: usize = 10;
const MAX_QUERIES_PER_PASS: usize = 5;

/// Discovery is enabled by configuring at least one query.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub reddit_queries: Vec<String>,
}

impl DiscoveryConfig {
    pub fn from_env() -> Self {
        let reddit_queries = std::env::var("CROWDRELAY_DISCOVERY_REDDIT_QUERIES")
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|query| !query.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self { reddit_queries }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.reddit_queries.is_empty()
    }
}

/// Static client configuration cannot fail; this error exists so a builder
/// regression surfaces as a startup error rather than a panic.
#[derive(Debug, thiserror::Error)]
#[error("reddit discovery HTTP client build failed")]
pub struct DiscoveryBuildError(#[from] reqwest::Error);

pub struct RedditDiscoveryWorker {
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    client: reqwest::Client,
    config: DiscoveryConfig,
    poll_interval: Duration,
    operation_timeout: Duration,
}

impl RedditDiscoveryWorker {
    pub fn new(
        pool: sqlx::PgPool,
        workspace_id: WorkspaceId,
        config: DiscoveryConfig,
        poll_interval: Duration,
        operation_timeout: Duration,
    ) -> Result<Self, DiscoveryBuildError> {
        // Static configuration cannot fail; a panic here would be a bug, so
        // the constructor surfaces it as an error like every other builder.
        let client = reqwest::Client::builder()
            .connect_timeout(operation_timeout.min(Duration::from_secs(10)))
            .timeout(operation_timeout)
            .user_agent(USER_AGENT)
            .build()
            .map_err(DiscoveryBuildError)?;
        Ok(Self {
            pool,
            workspace_id,
            client,
            config,
            poll_interval,
            operation_timeout,
        })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        if !self.config.enabled() {
            tracing::info!("reddit discovery disabled; no queries configured");
            return;
        }
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                _ = ticker.tick() => {
                    match timeout(self.operation_timeout * 2, self.run_once()).await {
                        Ok(Ok(imported)) if imported > 0 => {
                            tracing::info!(imported, "reddit discovery imported new places");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "reddit discovery sweep failed"),
                        Err(_) => tracing::warn!("reddit discovery sweep timed out"),
                    }
                }
            }
        }
    }

    async fn run_once(&self) -> Result<usize, DiscoveryError> {
        let repository = PostgresAudienceGraphRepository::new(self.pool.clone());
        let mut imported = 0_usize;
        for query in self.config.reddit_queries.iter().take(MAX_QUERIES_PER_PASS) {
            let subreddits = self.search_subreddits(query).await?;
            let inputs: Vec<UpsertPlaceInput> = subreddits
                .iter()
                .map(|sub| sub.to_place_input(self.workspace_id.into_uuid()))
                .collect();
            // One evidence row per place carries the raw listing payload.
            let mut evidence_by_index = std::collections::HashMap::new();
            for (index, sub) in subreddits.iter().enumerate() {
                evidence_by_index.insert(
                    index,
                    vec![EvidenceInput {
                        evidence_kind: "scan",
                        method: "reddit_public_search",
                        confidence_bp: 6_000,
                        payload: &sub.raw,
                    }],
                );
            }
            imported += repository
                .import_scan_batch(&inputs, &evidence_by_index)
                .await?
                .len();
            // Politeness gap between queries.
            tokio::time::sleep(REQUEST_SPACING).await;
        }
        Ok(imported)
    }

    async fn search_subreddits(
        &self,
        query: &str,
    ) -> Result<Vec<NormalizedSubreddit>, DiscoveryError> {
        let response = self
            .client
            .get("https://www.reddit.com/subreddits/search.json")
            .query(&[
                ("q", query),
                ("limit", &MAX_SUBREDDITS_PER_QUERY.to_string()),
                ("sort", "activity"),
            ])
            .send()
            .await
            .map_err(DiscoveryError::Network)?;
        if !response.status().is_success() {
            return Err(DiscoveryError::Status(response.status().as_u16()));
        }
        let body = response.bytes().await.map_err(DiscoveryError::Network)?;
        let document: RedditSearchResponse =
            serde_json::from_slice(&body).map_err(DiscoveryError::Payload)?;
        // NSFW listings are excluded at the adapter boundary, not filtered
        // later: the graph must be safe to show an operator by default.
        Ok(document
            .data
            .children
            .into_iter()
            .filter(|child| !child.data.over18)
            .map(|child| NormalizedSubreddit::from_listing(child.data))
            .collect())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("reddit discovery request failed")]
    Network(#[from] reqwest::Error),
    #[error("reddit returned HTTP {0}")]
    Status(u16),
    #[error("reddit payload was not the expected listing shape")]
    Payload(#[from] serde_json::Error),
    #[error("discovery persistence failed")]
    Database(#[from] sqlx::Error),
    #[error("discovery persistence failed")]
    Graph(#[from] crowdrelay_infra::audience_graph::AudienceGraphError),
}

#[derive(Debug, Deserialize)]
struct RedditSearchResponse {
    data: RedditListing,
}

#[derive(Debug, Deserialize)]
struct RedditListing {
    children: Vec<RedditChild>,
}

#[derive(Debug, Deserialize)]
struct RedditChild {
    data: RedditSubredditData,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RedditSubredditData {
    display_name_prefixed: String,
    url: String,
    title: String,
    #[serde(default)]
    public_description: String,
    subscribers: Option<u64>,
    #[serde(default)]
    over18: bool,
    lang: Option<String>,
}

/// The normalized slice of a listing that becomes one graph place.
#[derive(Debug)]
pub struct NormalizedSubreddit {
    pub name: String,
    pub url: String,
    pub display_name: String,
    pub description: String,
    pub subscribers: u64,
    pub raw: serde_json::Value,
}

impl NormalizedSubreddit {
    pub(crate) fn from_listing(data: RedditSubredditData) -> Self {
        let raw = serde_json::to_value(&data).unwrap_or(serde_json::Value::Null);
        Self {
            name: data.display_name_prefixed.clone(),
            url: format!("https://www.reddit.com{}", data.url.trim_end_matches('/')),
            display_name: data.title,
            description: data.public_description,
            subscribers: data.subscribers.unwrap_or(0),
            raw,
        }
    }

    /// Activity heuristic in basis points, banded on subscriber count. Deliberately
    /// coarse: it ranks discovery results, it does not pretend to be telemetry.
    pub fn activity_bp(&self) -> i32 {
        const BANDS: [(u64, i32); 6] = [
            (1_000, 2_000),
            (10_000, 3_500),
            (50_000, 5_000),
            (250_000, 6_500),
            (1_000_000, 8_000),
            (u64::MAX, 9_000),
        ];
        BANDS
            .iter()
            .find(|(threshold, _)| self.subscribers < *threshold)
            .map(|(_, value)| *value)
            .unwrap_or(2_000)
    }

    pub fn to_place_input<'a>(&'a self, workspace_id: Uuid) -> UpsertPlaceInput<'a> {
        UpsertPlaceInput {
            workspace_id,
            place_kind: crowdrelay_domain::audience_graph::PlaceKind::Subreddit,
            platform: "reddit",
            name: &self.display_name,
            url: &self.url,
            country_code: None,
            language: None,
            genres: &[],
            member_count: Some(i32::try_from(self.subscribers).unwrap_or(i32::MAX)),
            activity_bp: Some(self.activity_bp()),
            notes: Some(self.description.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::audience_graph::PlaceKind;

    fn sample_listing(subscribers: u64, over18: bool) -> RedditSubredditData {
        RedditSubredditData {
            display_name_prefixed: "r/Metal".to_owned(),
            url: "/r/Metal/".to_owned(),
            title: "All things Metal".to_owned(),
            public_description: "Heavy metal discussion".to_owned(),
            subscribers: Some(subscribers),
            over18,
            lang: Some("en".to_owned()),
        }
    }

    #[test]
    fn normalization_maps_the_graph_shape() {
        let normalized = NormalizedSubreddit::from_listing(sample_listing(420_000, false));
        assert_eq!(normalized.name, "r/Metal");
        assert_eq!(normalized.url, "https://www.reddit.com/r/Metal");
        assert_eq!(normalized.subscribers, 420_000);
        let input = normalized.to_place_input(Uuid::nil());
        assert_eq!(input.place_kind, PlaceKind::Subreddit);
        assert_eq!(input.platform, "reddit");
        assert_eq!(input.member_count, Some(420_000));
    }

    #[test]
    fn activity_bands_are_monotonic_and_bounded() {
        let mut previous = 0;
        for subscribers in [500u64, 5_000, 40_000, 200_000, 900_000, 5_000_000] {
            let normalized = NormalizedSubreddit::from_listing(sample_listing(subscribers, false));
            let activity = normalized.activity_bp();
            assert!(activity > previous, "{subscribers} must outrank {previous}");
            assert!((0..=10_000).contains(&activity));
            previous = activity;
        }
    }

    #[test]
    fn config_is_disabled_without_queries() {
        // SAFETY-free check via the pure branch: constructing with no env var
        // happens through from_env in production; here we pin the semantics.
        let config = DiscoveryConfig {
            reddit_queries: vec![],
        };
        assert!(!config.enabled());
        let config = DiscoveryConfig {
            reddit_queries: vec!["metal".to_owned()],
        };
        assert!(config.enabled());
    }
}
