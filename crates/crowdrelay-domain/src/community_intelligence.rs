//! Community Intelligence — Observation Layer policy.
//!
//! Pure policy module for structured community observations. This module
//! holds FACTS, not INTERPRETATIONS. Sentiment, audience_affinity,
//! promotion_norm, and trend are interpretations that belong in a future
//! CommunitySignal layer, not here. Promotion policy lives in
//! `discovery_place_rules`, not here.
//!
//! The observation layer sits on top of the existing raw evidence layer
//! (`discovery_place_evidence`). Raw HTML/JSON payloads still land there;
//! this module defines the normalized measurements and extracted entities
//! that the Brain can reason over.

use serde::{Deserialize, Serialize};

/// The kinds of entities that can be extracted from a community observation.
/// Stored as a CHECK constraint in `community_entities.entity_type`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Artist,
    Band,
    Topic,
    Genre,
    Label,
}

impl EntityType {
    pub const ALL: [EntityType; 5] = [
        EntityType::Artist,
        EntityType::Band,
        EntityType::Topic,
        EntityType::Genre,
        EntityType::Label,
    ];

    /// Maps to the database CHECK constraint values.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            EntityType::Artist => "artist",
            EntityType::Band => "band",
            EntityType::Topic => "topic",
            EntityType::Genre => "genre",
            EntityType::Label => "label",
        }
    }

    /// Parses from the database string. Unknown values are rejected
    /// rather than collapsing into a catch-all.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "artist" => Some(EntityType::Artist),
            "band" => Some(EntityType::Band),
            "topic" => Some(EntityType::Topic),
            "genre" => Some(EntityType::Genre),
            "label" => Some(EntityType::Label),
            _ => None,
        }
    }
}

/// Maximum observation quality — fully structured extraction.
pub const OBSERVATION_QUALITY_MAX: i32 = 10_000;

/// Maximum entity strength — highest observed prominence.
pub const ENTITY_STRENGTH_MAX: i32 = 10_000;

/// Maximum lengths for provenance fields (must match DB CHECK constraints).
pub const MAX_SOURCE_LEN: usize = 64;
pub const MAX_SOURCE_URL_LEN: usize = 512;
pub const MAX_COLLECTOR_VERSION_LEN: usize = 32;
pub const MAX_ENTITY_REF_LEN: usize = 200;

/// A structured community observation — one snapshot of a community's
/// measurable facts at a point in time.
///
/// This struct does NOT carry `workspace_id` or `place_id` — those are
/// provided by the caller (the worker knows which place it observed).
/// The observation itself is just the measurements + provenance.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommunityObservation {
    /// Where this observation came from (e.g. "brutalland").
    pub source: String,
    /// The URL that was fetched.
    pub source_url: String,
    /// Version of the collector/parser (e.g. "brutalland-v1").
    pub collector_version: String,
    /// Normalized measurements — facts, not interpretations.
    /// e.g. `{"online_users": 59, "total_posts": 33515, "posts_last_24h": 12}`
    pub raw_activity_metrics: serde_json::Value,
    /// How reliable is this measurement?
    /// 0 = parser may have failed, 10000 = fully structured extraction.
    pub observation_quality: i32,
}

/// An entity extracted from a community observation (artist, band, topic,
/// genre, or label).
///
/// `entity_ref` is normalized source-level identity, NOT a foreign key to
/// fanbase/artist records. Entity resolution is a future Sprint C concern.
///
/// `strength` is observed prominence only (mention count, thread count,
/// section prominence). It is NOT relevance, affinity, influence, or
/// recommendation score.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommunityEntity {
    pub entity_type: EntityType,
    /// Normalized source-level identity (e.g. "Spiritbox", "djent",
    /// "Season of Mist").
    pub entity_ref: String,
    /// Observed prominence, 0-10000.
    pub strength: i32,
}

/// Validation error for community intelligence data.
#[derive(Debug, thiserror::Error)]
pub enum CommunityIntelligenceError {
    #[error("source must be 1-{MAX_SOURCE_LEN} characters, got {0}")]
    SourceLength(usize),
    #[error("source_url must be 0-{MAX_SOURCE_URL_LEN} characters, got {0}")]
    SourceUrlLength(usize),
    #[error("collector_version must be 1-{MAX_COLLECTOR_VERSION_LEN} characters, got {0}")]
    CollectorVersionLength(usize),
    #[error("observation_quality must be 0-{OBSERVATION_QUALITY_MAX}, got {0}")]
    ObservationQualityOutOfRange(i32),
    #[error("entity_ref must be 1-{MAX_ENTITY_REF_LEN} characters, got {0}")]
    EntityRefLength(usize),
    #[error("entity strength must be 0-{ENTITY_STRENGTH_MAX}, got {0}")]
    StrengthOutOfRange(i32),
}

/// Validates a community observation's provenance and quality fields.
pub fn validate_observation(obs: &CommunityObservation) -> Result<(), CommunityIntelligenceError> {
    let source_len = obs.source.trim().len();
    if source_len == 0 || source_len > MAX_SOURCE_LEN {
        return Err(CommunityIntelligenceError::SourceLength(obs.source.len()));
    }
    if obs.source_url.len() > MAX_SOURCE_URL_LEN {
        return Err(CommunityIntelligenceError::SourceUrlLength(
            obs.source_url.len(),
        ));
    }
    let version_len = obs.collector_version.trim().len();
    if version_len == 0 || version_len > MAX_COLLECTOR_VERSION_LEN {
        return Err(CommunityIntelligenceError::CollectorVersionLength(
            obs.collector_version.len(),
        ));
    }
    if !(0..=OBSERVATION_QUALITY_MAX).contains(&obs.observation_quality) {
        return Err(CommunityIntelligenceError::ObservationQualityOutOfRange(
            obs.observation_quality,
        ));
    }
    Ok(())
}

/// Validates a community entity's fields.
pub fn validate_entity(entity: &CommunityEntity) -> Result<(), CommunityIntelligenceError> {
    let ref_len = entity.entity_ref.trim().len();
    if ref_len == 0 || ref_len > MAX_ENTITY_REF_LEN {
        return Err(CommunityIntelligenceError::EntityRefLength(
            entity.entity_ref.len(),
        ));
    }
    if !(0..=ENTITY_STRENGTH_MAX).contains(&entity.strength) {
        return Err(CommunityIntelligenceError::StrengthOutOfRange(
            entity.strength,
        ));
    }
    Ok(())
}

/// Normalizes a raw mention count to the 0-10000 strength scale.
/// Uses a square-root curve so that 1 mention ≠ strength 1 and 100 mentions
/// ≠ strength 100 — the scale rewards volume but dampens outliers.
///
/// `strength` = observed prominence only. It is NOT relevance, affinity,
/// influence, or recommendation score.
pub fn normalize_strength(mention_count: u32) -> i32 {
    if mention_count == 0 {
        return 0;
    }
    let scaled = (mention_count as f64).sqrt() * (ENTITY_STRENGTH_MAX as f64 / 100.0);
    (scaled.round() as i32).min(ENTITY_STRENGTH_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_type_round_trips_through_db_str() {
        for et in EntityType::ALL {
            let s = et.as_db_str();
            assert_eq!(EntityType::from_db_str(s), Some(et));
        }
    }

    #[test]
    fn entity_type_rejects_unknown() {
        assert_eq!(EntityType::from_db_str("unknown"), None);
    }

    #[test]
    fn validate_observation_rejects_empty_source() {
        let obs = CommunityObservation {
            source: "   ".to_owned(),
            source_url: "https://example.com".to_owned(),
            collector_version: "v1".to_owned(),
            raw_activity_metrics: serde_json::json!({}),
            observation_quality: 5000,
        };
        assert!(validate_observation(&obs).is_err());
    }

    #[test]
    fn validate_observation_rejects_quality_out_of_range() {
        let obs = CommunityObservation {
            source: "test".to_owned(),
            source_url: "https://example.com".to_owned(),
            collector_version: "v1".to_owned(),
            raw_activity_metrics: serde_json::json!({}),
            observation_quality: OBSERVATION_QUALITY_MAX + 1,
        };
        assert!(validate_observation(&obs).is_err());
    }

    #[test]
    fn validate_observation_accepts_valid_input() {
        let obs = CommunityObservation {
            source: "brutalland".to_owned(),
            source_url: "https://brutalland.pl/".to_owned(),
            collector_version: "brutalland-v1".to_owned(),
            raw_activity_metrics: serde_json::json!({"online_users": 59}),
            observation_quality: OBSERVATION_QUALITY_MAX,
        };
        assert!(validate_observation(&obs).is_ok());
    }

    #[test]
    fn validate_entity_rejects_empty_ref() {
        let entity = CommunityEntity {
            entity_type: EntityType::Artist,
            entity_ref: "  ".to_owned(),
            strength: 100,
        };
        assert!(validate_entity(&entity).is_err());
    }

    #[test]
    fn normalize_strength_zero_stays_zero() {
        assert_eq!(normalize_strength(0), 0);
    }

    #[test]
    fn normalize_strength_capped_at_max() {
        assert_eq!(normalize_strength(u32::MAX), ENTITY_STRENGTH_MAX);
    }

    #[test]
    fn normalize_strength_dampens_outliers() {
        // 100 mentions → sqrt(100) * 100 = 1000, not 10000
        let s = normalize_strength(100);
        assert!(s > 0 && s < ENTITY_STRENGTH_MAX);
    }
}
