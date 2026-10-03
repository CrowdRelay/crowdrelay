//! Durable identities FAN SCOUT must never treat as prospects.
//!
//! This is intentionally an explicit operator declaration, not a heuristic.
//! The production failure that motivated it was the owner's own test account
//! becoming a prospect and two test replies tripping the global scout-lane
//! over-rate halt. An exclusion prevents future observation and suppresses an
//! already-live matching prospect, but never deletes history and never clears a
//! standing breach automatically — a human still acknowledges the incident.

use crowdrelay_domain::fan_prospect::{
    ProspectIdentityExclusionReason, ProspectIdentityKind, normalize_platform,
};
use serde::Serialize;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct ProspectIdentityExclusion {
    pub id: Uuid,
    pub kind: String,
    pub platform: String,
    pub value: String,
    pub reason: String,
    pub recorded_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
}

pub(crate) async fn identity_is_excluded(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    kind: ProspectIdentityKind,
    platform: &str,
    value: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM fan_prospect_identity_exclusions
             WHERE workspace_id=$1 AND kind=$2 AND platform=$3 AND value=$4
         )",
    )
    .bind(workspace_id)
    .bind(kind.as_str())
    .bind(platform)
    .bind(value)
    .fetch_one(&mut **transaction)
    .await
}

/// Adds or refreshes one exclusion and immediately suppresses an existing live
/// prospect that owns the same identity. Converted/refused/suppressed history
/// is never rewritten.
pub async fn exclude_identity(
    pool: &PgPool,
    workspace_id: Uuid,
    kind: ProspectIdentityKind,
    platform: &str,
    value: &str,
    reason: ProspectIdentityExclusionReason,
    recorded_by: &str,
) -> Result<Option<ProspectIdentityExclusion>, sqlx::Error> {
    let Some(platform) = normalize_platform(platform) else {
        return Ok(None);
    };
    let Some(value) = kind.normalize(value) else {
        return Ok(None);
    };
    let recorded_by = recorded_by.trim();
    if recorded_by.is_empty() || recorded_by.chars().count() > 120 {
        return Ok(None);
    }

    let mut transaction = pool.begin().await?;
    let exclusion = sqlx::query_as::<_, ProspectIdentityExclusion>(
        r#"
        INSERT INTO fan_prospect_identity_exclusions
            (workspace_id, kind, platform, value, reason, recorded_by)
        VALUES ($1,$2,$3,$4,$5,$6)
        ON CONFLICT (workspace_id, kind, platform, value) DO UPDATE SET
            reason=EXCLUDED.reason,
            recorded_by=EXCLUDED.recorded_by,
            recorded_at=now()
        RETURNING id, kind, platform, value, reason, recorded_by, recorded_at
        "#,
    )
    .bind(workspace_id)
    .bind(kind.as_str())
    .bind(&platform)
    .bind(&value)
    .bind(reason.as_str())
    .bind(recorded_by)
    .fetch_one(&mut *transaction)
    .await?;

    let status_reason = format!("identity_exclusion:{}", reason.as_str());
    sqlx::query(
        r#"
        UPDATE fan_prospects AS prospect
        SET status='suppressed', status_reason=$5, updated_at=now()
        FROM person_identities AS identity
        WHERE prospect.workspace_id=$1
          AND identity.workspace_id=prospect.workspace_id
          AND identity.person_id=prospect.person_id
          AND identity.kind=$2
          AND identity.platform=$3
          AND identity.value=$4
          AND prospect.status NOT IN ('converted','refused','suppressed')
        "#,
    )
    .bind(workspace_id)
    .bind(kind.as_str())
    .bind(&platform)
    .bind(&value)
    .bind(status_reason)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    Ok(Some(exclusion))
}

pub async fn list_identity_exclusions(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<ProspectIdentityExclusion>, sqlx::Error> {
    sqlx::query_as::<_, ProspectIdentityExclusion>(
        "SELECT id, kind, platform, value, reason, recorded_by, recorded_at
         FROM fan_prospect_identity_exclusions
         WHERE workspace_id=$1
         ORDER BY recorded_at DESC, id",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await
}

/// Removes the future-ingest exclusion only. Any prospect already suppressed by
/// it stays suppressed: deleting a guard is not permission to resurrect a
/// person the machine previously decided not to contact.
pub async fn delete_identity_exclusion(
    pool: &PgPool,
    workspace_id: Uuid,
    id: Uuid,
) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM fan_prospect_identity_exclusions WHERE workspace_id=$1 AND id=$2")
            .bind(workspace_id)
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}
