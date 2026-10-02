//! Durable FAN SCOUT prospect/evidence repository.
//!
//! Discovery can observe a public person repeatedly without ever turning them
//! into a first-party fan. The only promotion-shaped operation here is
//! link_verified_fan, and it can only point at an existing active fan with an
//! already-verified first-party identifier.

use crowdrelay_domain::{
    FanId, FanProspectId, WorkspaceId,
    fan_scout::{FanProspectIdentity, FanProspectObservationKind},
};
use serde_json::Value;
use sqlx::PgPool;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
pub enum FanScoutStoreError {
    #[error("invalid fan-scout input: {0}")]
    InvalidInput(&'static str),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug)]
pub struct ObserveProspectRequest {
    pub workspace_id: WorkspaceId,
    pub identity: FanProspectIdentity,
    pub display_name: Option<String>,
    pub profile_url: Option<String>,
    pub observation_kind: FanProspectObservationKind,
    pub source_kind: String,
    pub source_id: String,
    pub source_url: Option<String>,
    pub evidence: Value,
    pub observed_at: OffsetDateTime,
}

fn bounded_optional(
    value: Option<String>,
    max_chars: usize,
) -> Result<Option<String>, FanScoutStoreError> {
    value
        .map(|value| {
            let value = value.trim().to_owned();
            if value.is_empty() || value.chars().count() > max_chars {
                Err(FanScoutStoreError::InvalidInput(
                    "optional text out of bounds",
                ))
            } else {
                Ok(value)
            }
        })
        .transpose()
}

fn valid_source_kind(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'.' | b':' | b'-')
        })
}

/// Records one public person plus one sourced observation idempotently.
///
/// Re-observation refreshes names and last-seen time but intentionally never
/// writes status: a later scout cannot erase a refusal/suppression.
///
/// # Errors
/// Invalid bounded input or database failure.
pub async fn observe(
    pool: &PgPool,
    request: ObserveProspectRequest,
) -> Result<FanProspectId, FanScoutStoreError> {
    if !valid_source_kind(&request.source_kind) {
        return Err(FanScoutStoreError::InvalidInput("invalid source_kind"));
    }
    let source_id = request.source_id.trim().to_owned();
    if source_id.is_empty() || source_id.chars().count() > 512 {
        return Err(FanScoutStoreError::InvalidInput("invalid source_id"));
    }
    if !request.evidence.is_object() {
        return Err(FanScoutStoreError::InvalidInput(
            "evidence must be a JSON object",
        ));
    }
    let display_name = bounded_optional(request.display_name, 200)?;
    let profile_url = bounded_optional(request.profile_url, 1000)?;
    let source_url = bounded_optional(request.source_url, 1000)?;
    let workspace_id = request.workspace_id.into_uuid();

    let mut tx = pool.begin().await?;
    let prospect_id: uuid::Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO scout_prospects (
            workspace_id, platform, identity_kind, external_identity,
            identity_key, display_name, profile_url, first_seen_at, last_seen_at
        )
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8)
        ON CONFLICT (workspace_id, platform, identity_kind, identity_key)
        DO UPDATE SET
            external_identity = EXCLUDED.external_identity,
            display_name = COALESCE(EXCLUDED.display_name, scout_prospects.display_name),
            profile_url = COALESCE(EXCLUDED.profile_url, scout_prospects.profile_url),
            first_seen_at = LEAST(scout_prospects.first_seen_at, EXCLUDED.first_seen_at),
            last_seen_at = GREATEST(scout_prospects.last_seen_at, EXCLUDED.last_seen_at),
            updated_at = now()
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(request.identity.platform())
    .bind(request.identity.kind().as_str())
    .bind(request.identity.external_identity())
    .bind(request.identity.identity_key())
    .bind(display_name)
    .bind(profile_url)
    .bind(request.observed_at)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO scout_prospect_observations (
            workspace_id, prospect_id, observation_kind, source_kind,
            source_id, source_url, evidence, observed_at
        )
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
        ON CONFLICT (
            workspace_id, prospect_id, observation_kind, source_kind, source_id
        ) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(prospect_id)
    .bind(request.observation_kind.as_str())
    .bind(request.source_kind)
    .bind(source_id)
    .bind(source_url)
    .bind(request.evidence)
    .bind(request.observed_at)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(FanProspectId::from_uuid(prospect_id))
}

/// Links a prospect to an already verified first-party fan.
///
/// This never creates a fans row. It only succeeds when the fan is active,
/// not deleted, belongs to the same workspace and has at least one verified
/// identifier. A refused/suppressed prospect is not resurrected by conversion
/// attribution.
///
/// # Errors
/// Database failure.
pub async fn link_verified_fan(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    prospect_id: FanProspectId,
    fan_id: FanId,
    now: OffsetDateTime,
) -> Result<bool, FanScoutStoreError> {
    let linked = sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        UPDATE scout_prospects AS prospect
        SET linked_fan_id = $3,
            status = 'converted',
            converted_at = COALESCE(prospect.converted_at, $4),
            last_seen_at = GREATEST(prospect.last_seen_at, $4),
            updated_at = $4
        WHERE prospect.workspace_id = $1
          AND prospect.id = $2
          AND prospect.status NOT IN ('refused', 'suppressed')
          AND EXISTS (
              SELECT 1
              FROM fans AS fan
              WHERE fan.workspace_id = $1
                AND fan.id = $3
                AND fan.status = 'active'
                AND fan.deleted_at IS NULL
                AND EXISTS (
                    SELECT 1
                    FROM fan_identifiers AS identifier
                    WHERE identifier.workspace_id = fan.workspace_id
                      AND identifier.fan_id = fan.id
                      AND identifier.verified_at IS NOT NULL
                )
          )
        RETURNING prospect.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(prospect_id.into_uuid())
    .bind(fan_id.into_uuid())
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(linked.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_is_a_small_machine_key_not_free_text() {
        assert!(valid_source_kind("community_comment"));
        assert!(valid_source_kind("youtube.comment:v1"));
        assert!(!valid_source_kind("Community Comment"));
        assert!(!valid_source_kind(""));
    }
}
