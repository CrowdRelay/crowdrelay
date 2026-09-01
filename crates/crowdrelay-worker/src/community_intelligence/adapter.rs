//! Source adapter trait — the hexagonal boundary between fetching/parsing
//! and persistence.
//!
//! The parser CANNOT write — it only returns data. The worker handles
//! persistence. This keeps the adapter testable in isolation and prevents
//! a parser bug from corrupting the database.

use std::time::Duration;

use crowdrelay_domain::community_intelligence::{CommunityEntity, EntityType};

/// A minimal place row for the source adapter. The adapter only needs
/// the URL and metadata to fetch a community surface — it does not need
/// the full joined PlaceRow with rules and outreach state.
pub struct AdapterPlace {
    pub id: uuid::Uuid,
    pub workspace_id: uuid::Uuid,
    pub platform: String,
    pub name: String,
    pub url: String,
}

/// A source adapter fetches and parses one community surface.
/// It returns data; the worker handles persistence.
#[async_trait::async_trait]
pub trait SourceAdapter: Send + Sync {
    /// Unique identifier for this adapter (e.g. "brutalland").
    fn id(&self) -> &str;

    /// Recommended interval between observations for this source.
    fn recommended_interval(&self) -> Duration;

    /// Rate limit policy: max concurrency, backoff on 429.
    fn rate_limit_policy(&self) -> RateLimitPolicy;

    /// Fetch and parse one community surface.
    /// Returns a ParsedObservation on success, an error on failure.
    /// NEVER writes to the database — that's the worker's job.
    async fn fetch(&self, place: &AdapterPlace) -> Result<ParsedObservation, AdapterError>;
}

/// Rate limit policy for a source adapter.
pub struct RateLimitPolicy {
    pub max_concurrency: usize,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

/// A parsed observation — the output of a source adapter's fetch.
/// This is the raw data that the worker validates and persists.
pub struct ParsedObservation {
    pub source: String,
    pub source_url: String,
    pub collector_version: String,
    pub raw_activity_metrics: serde_json::Value,
    pub observation_quality: i32,
    pub entities: Vec<ParsedEntity>,
}

/// A parsed entity — extracted from the community surface.
pub struct ParsedEntity {
    pub entity_type: EntityType,
    pub entity_ref: String,
    pub strength: i32,
}

impl From<ParsedEntity> for CommunityEntity {
    fn from(p: ParsedEntity) -> Self {
        CommunityEntity {
            entity_type: p.entity_type,
            entity_ref: p.entity_ref,
            strength: p.strength,
        }
    }
}

/// Errors that a source adapter can return.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("HTTP fetch failed: {0}")]
    HttpFetch(String),
    #[error("HTTP status {status} from {url}")]
    HttpStatus { status: u16, url: String },
    #[error("parse failed: {0}")]
    Parse(String),
    #[error("page structure changed — markers not found (fail closed)")]
    StructureChanged,
    #[error("timeout after {0:?}")]
    Timeout(Duration),
}
