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
use crowdrelay_infra::reddit_proxy::read_reddit_proxy_from_db;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, timeout},
};
use uuid::Uuid;

const USER_AGENT: &str = "CrowdRelay/1.0 audience-graph-discovery";

/// Namespace for the HMAC-derived management token, matching the agents
/// service's `auth.ts` and the control plane's `tenant_area_client.rs`.
const AGENT_AUTH_NAMESPACE: &[u8] = b"crowdrelay-control-plane-v1:";

/// Derives a per-workspace bearer token for the agents service.
/// `token = hex(HMAC-SHA256(master_key, namespace + workspace_id))`
pub(crate) fn derive_agent_token(master_key: &str, workspace_id: Uuid) -> String {
    // HMAC-SHA256 accepts any key length; this never fails.
    let mut mac = match <Hmac<Sha256> as Mac>::new_from_slice(master_key.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return String::new(),
    };
    mac.update(AGENT_AUTH_NAMESPACE);
    mac.update(workspace_id.to_string().as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Reddit's unauthenticated JSON API tolerates roughly one request per second
/// per client; two seconds keeps the sweep polite without making it useless.
const REQUEST_SPACING: Duration = Duration::from_secs(2);
const MAX_SUBREDDITS_PER_QUERY: usize = 10;
const MAX_QUERIES_PER_PASS: usize = 5;
/// How often the worker checks `reddit_proxy_state` for a new proxy.
const PROXY_REFRESH_INTERVAL: Duration = Duration::from_secs(300); // 5 min
/// Watchdog for one full sweep. A sweep may trigger browser scrapes in the
/// agents service (possible login + navigation = minutes), so the old
/// `operation_timeout * 2` cap (10s) would cancel sweeps mid-flight and
/// leave nothing imported. This only guards against a permanently hung cycle.
const SWEEP_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(1800);
/// Scrape POSTs can drive a browser login server-side; give them room.
/// Overrides the client-wide operation timeout for that one request.
const AGENTS_SCRAPE_TIMEOUT: Duration = Duration::from_secs(300);
/// Plain agents reads (no browser work involved).
const AGENTS_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds a reqwest client with an optional proxy. Shared between the
/// constructor (initial build) and the run-loop proxy refresh.
fn build_reddit_client(
    proxy_url: Option<&str>,
    operation_timeout: Duration,
) -> Result<reqwest::Client, DiscoveryBuildError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(operation_timeout.min(Duration::from_secs(10)))
        .timeout(operation_timeout)
        .user_agent(USER_AGENT);
    if let Some(url) = proxy_url {
        let proxy =
            reqwest::Proxy::all(url).map_err(|e| DiscoveryBuildError::Proxy(e.to_string()))?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(DiscoveryBuildError::Client)
}

/// Resolves the effective proxy URL: DB proxy (written by the sidecar)
/// takes precedence, falling back to the env var override.
async fn resolve_proxy_url(pool: &sqlx::PgPool, env_proxy: &Option<String>) -> Option<String> {
    if let Some(db_proxy) = read_reddit_proxy_from_db(pool).await {
        return Some(db_proxy);
    }
    env_proxy.clone()
}

/// Discovery is enabled by configuring at least one query.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub reddit_queries: Vec<String>,
    /// Optional proxy URL for Reddit requests (bypasses IP-level 403 blocks).
    pub proxy_url: Option<String>,
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
        let proxy_url = std::env::var("CROWDRELAY_REDDIT_PROXY_URL")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        Self {
            reddit_queries,
            proxy_url,
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.reddit_queries.is_empty()
    }
}

/// Static client configuration cannot fail; this error exists so a builder
/// regression surfaces as a startup error rather than a panic.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryBuildError {
    #[error("reddit discovery HTTP client build failed")]
    Client(#[from] reqwest::Error),
    #[error("invalid proxy URL: {0}")]
    Proxy(String),
}

pub struct RedditDiscoveryWorker {
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    client: reqwest::Client,
    config: DiscoveryConfig,
    poll_interval: Duration,
    operation_timeout: Duration,
    /// Base URL of the agents service (for Reddit cookie fetching).
    agent_service_url: String,
    /// Auth key for the agents service (HMAC-derived management token).
    agent_service_auth_key: Option<String>,
}

impl RedditDiscoveryWorker {
    pub fn new(
        pool: sqlx::PgPool,
        workspace_id: WorkspaceId,
        config: DiscoveryConfig,
        poll_interval: Duration,
        operation_timeout: Duration,
        agent_service_url: String,
        agent_service_auth_key: Option<String>,
    ) -> Result<Self, DiscoveryBuildError> {
        let client = build_reddit_client(config.proxy_url.as_deref(), operation_timeout)?;
        Ok(Self {
            pool,
            workspace_id,
            client,
            config,
            poll_interval,
            operation_timeout,
            agent_service_url,
            agent_service_auth_key,
        })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        if !self.config.enabled() {
            tracing::info!("reddit discovery disabled; no queries configured");
            return;
        }
        let operation_timeout = self.operation_timeout;
        let mut client = self.client.clone(); // cheap — Arc-backed
        let mut current_proxy = self.config.proxy_url.clone();
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut proxy_timer = tokio::time::interval(PROXY_REFRESH_INTERVAL);
        proxy_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        proxy_timer.tick().await; // skip first immediate tick
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                _ = proxy_timer.tick() => {
                    let new_proxy = resolve_proxy_url(&self.pool, &self.config.proxy_url).await;
                    if new_proxy != current_proxy {
                        match build_reddit_client(new_proxy.as_deref(), operation_timeout) {
                            Ok(new_client) => {
                                tracing::info!(
                                    old = current_proxy.as_deref().unwrap_or("direct"),
                                    new = new_proxy.as_deref().unwrap_or("direct"),
                                    "reddit discovery proxy changed, rebuilding HTTP client"
                                );
                                client = new_client;
                                current_proxy = new_proxy;
                            }
                            Err(error) => {
                                tracing::warn!(error = %error, "failed to rebuild client with new proxy, keeping old client");
                            }
                        }
                    }
                }
                _ = ticker.tick() => {
                    match timeout(SWEEP_WATCHDOG_TIMEOUT, self.run_once(&client)).await {
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

    async fn run_once(&self, client: &reqwest::Client) -> Result<usize, DiscoveryError> {
        let repository = PostgresAudienceGraphRepository::new(self.pool.clone());
        let mut imported = 0_usize;
        for query in self.config.reddit_queries.iter().take(MAX_QUERIES_PER_PASS) {
            // Skip queries that fail (agents service unreachable, Reddit
            // 403, etc.) instead of aborting the entire sweep. The next
            // sweep (6h interval) will retry. This way a single failed
            // query doesn't prevent processing of the remaining queries.
            let subreddits = match self.search_subreddits(client, query).await {
                Ok(subs) => subs,
                Err(error) => {
                    tracing::warn!(query, error = %error, "discovery query failed, skipping to next query");
                    continue;
                }
            };
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
                        method: sub.method,
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

    /// Fetches Reddit session cookies from the agents service (obtained by
    /// the Playwright scraper via Google OAuth). Returns None if the agents
    /// service is unreachable or no cookies are stored — the caller falls
    /// back to an unauthenticated request.
    async fn fetch_reddit_cookies(&self, client: &reqwest::Client) -> Option<String> {
        let auth_key = self.agent_service_auth_key.as_ref()?;
        let ws = self.workspace_id.into_uuid();
        let token = derive_agent_token(auth_key, ws);
        let url = format!("{}/reddit/cookies", self.agent_service_url);
        let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Workspace-Id", ws.to_string())
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        let cookies = body.get("cookies")?.as_array()?;
        let cookie_str = cookies
            .iter()
            .filter_map(|c| {
                let name = c.get("name")?.as_str()?;
                let value = c.get("value")?.as_str()?;
                Some(format!("{name}={value}"))
            })
            .collect::<Vec<_>>()
            .join("; ");
        if cookie_str.is_empty() {
            None
        } else {
            Some(cookie_str)
        }
    }

    /// Reads stored scrape results from the agents service. Ok(None) means
    /// "no results for this query (yet)" — the caller falls back.
    async fn fetch_scrape_results(
        &self,
        client: &reqwest::Client,
        auth_key: &str,
        query: &str,
    ) -> Result<Option<Vec<AgentsScrapeRow>>, DiscoveryError> {
        let ws = self.workspace_id.into_uuid();
        let url = format!("{}/reddit/scrape/results", self.agent_service_url);
        let response = client
            .get(&url)
            .query(&[
                ("query", query),
                ("limit", &MAX_SUBREDDITS_PER_QUERY.to_string()),
            ])
            .header("Authorization", format!("Bearer {auth_key}"))
            .header("X-Workspace-Id", ws.to_string())
            .timeout(AGENTS_READ_TIMEOUT)
            .send()
            .await
            .map_err(DiscoveryError::Network)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            if status == 429 || status >= 500 {
                // Transient failure — return an error so the caller skips
                // this query instead of hammering the agents service with
                // a scrape trigger or falling back to a likely-403 direct path.
                return Err(DiscoveryError::Status(status));
            }
            // 404 or other 4xx — no results for this query yet.
            tracing::warn!(status, body = %body, "agents scrape-results read failed");
            return Ok(None);
        }
        let rows: Vec<AgentsScrapeRow> = response.json().await?;
        Ok(Some(rows))
    }

    /// Asks the agents service to scrape this query through its logged-in
    /// browser. Returns false when the scrape could not run (service down,
    /// no credentials) — the caller then falls back to the direct path.
    async fn trigger_agents_scrape(
        &self,
        client: &reqwest::Client,
        auth_key: &str,
        query: &str,
    ) -> bool {
        let ws = self.workspace_id.into_uuid();
        let url = format!("{}/reddit/scrape", self.agent_service_url);
        let payload = serde_json::json!({
            "queries": [query],
            "limit": MAX_SUBREDDITS_PER_QUERY,
        });
        let response = match client
            .post(&url)
            .header("Authorization", format!("Bearer {auth_key}"))
            .header("X-Workspace-Id", ws.to_string())
            .json(&payload)
            .timeout(AGENTS_SCRAPE_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(error = %error, "agents scrape trigger failed");
                return false;
            }
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(status = status.as_u16(), body = %body, "agents scrape trigger rejected");
            return false;
        }
        true
    }

    /// Subreddit search through the agents service: read stored browser
    /// scrape results; when empty, trigger one scrape and re-read. Ok(None)
    /// means the agents path produced nothing (or was unreachable) and the
    /// caller should fall back to the direct Reddit request. Network errors
    /// are logged and treated as "no results" so the direct-Reddit fallback
    /// is always reached.
    async fn search_via_agents(
        &self,
        client: &reqwest::Client,
        query: &str,
    ) -> Result<Option<Vec<NormalizedSubreddit>>, DiscoveryError> {
        let Some(auth_key) = self.agent_service_auth_key.as_deref() else {
            return Ok(None);
        };
        let ws = self.workspace_id.into_uuid();
        let token = derive_agent_token(auth_key, ws);

        let rows = match self.fetch_scrape_results(client, &token, query).await {
            Ok(Some(rows)) if !rows.is_empty() => rows,
            Ok(_) => {
                // Nothing stored yet — scrape this query through the browser.
                if !self.trigger_agents_scrape(client, &token, query).await {
                    return Ok(None);
                }
                match self.fetch_scrape_results(client, &token, query).await {
                    Ok(Some(rows)) if !rows.is_empty() => rows,
                    _ => return Ok(None),
                }
            }
            Err(error) => {
                // Agents service unreachable — do NOT fall back to the
                // direct Reddit path (it 403s unauthenticated requests).
                // Propagate the error so the caller can skip this query
                // and retry on the next sweep instead of burning a
                // direct-Reddit request that will always fail.
                tracing::warn!(error = %error, "agents scrape-results read failed, skipping query");
                return Err(error);
            }
        };

        // NSFW rows are excluded at the adapter boundary (defensive — the
        // agents service already filters them before storing).
        let subreddits: Vec<NormalizedSubreddit> = rows
            .into_iter()
            .filter(|row| !row.over18)
            .map(NormalizedSubreddit::from_scrape_row)
            .collect();
        if subreddits.is_empty() {
            return Ok(None);
        }
        tracing::debug!(query, count = subreddits.len(), "agents scrape results");
        Ok(Some(subreddits))
    }

    async fn search_subreddits(
        &self,
        client: &reqwest::Client,
        query: &str,
    ) -> Result<Vec<NormalizedSubreddit>, DiscoveryError> {
        // Browser-first: the agents service scrapes Reddit with a real
        // logged-in session (the only access path that still works).
        if let Some(subreddits) = self.search_via_agents(client, query).await? {
            return Ok(subreddits);
        }

        // Legacy direct path — kept as fallback for deployments without the
        // agents service. Reddit 403s unauthenticated requests, so this is
        // effectively dark in production, but it must not regress for anyone
        // who has it working (e.g. via proxy).
        let mut request = client
            .get("https://www.reddit.com/subreddits/search.json")
            .query(&[
                ("q", query),
                ("limit", &MAX_SUBREDDITS_PER_QUERY.to_string()),
                ("sort", "activity"),
            ]);
        // Attach Reddit session cookies if available (bypasses JS challenge).
        if let Some(cookie_header) = self.fetch_reddit_cookies(client).await {
            request = request.header("Cookie", cookie_header);
        }
        let response = request.send().await.map_err(DiscoveryError::Network)?;
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

/// A row from the agents service's `reddit_scrape_results` table, returned
/// by `GET /reddit/scrape/results`. The agents service stores browser-scraped
/// subreddit listings; this struct deserializes them for normalization.
#[derive(Debug, Deserialize)]
pub(crate) struct AgentsScrapeRow {
    subreddit_name: String,
    display_name: String,
    #[serde(default)]
    description: String,
    subscribers: Option<u64>,
    url: String,
    #[serde(default, rename = "over18")]
    over18: bool,
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
    pub method: &'static str,
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
            method: "reddit_public_search",
        }
    }

    /// Converts a row from the agents service's browser scrape results into
    /// the normalized form. The agents service stores the subreddit name
    /// without the `r/` prefix, so we add it here for consistency with
    /// `from_listing`.
    pub(crate) fn from_scrape_row(row: AgentsScrapeRow) -> Self {
        let name = if row.subreddit_name.starts_with("r/") {
            row.subreddit_name
        } else {
            format!("r/{}", row.subreddit_name)
        };
        Self {
            name,
            url: row.url,
            display_name: row.display_name,
            description: row.description,
            subscribers: row.subscribers.unwrap_or(0),
            raw: serde_json::Value::Null,
            method: "agents_browser_scrape",
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
            proxy_url: None,
        };
        assert!(!config.enabled());
        let config = DiscoveryConfig {
            reddit_queries: vec!["metal".to_owned()],
            proxy_url: None,
        };
        assert!(config.enabled());
    }
}
