//! Persistence for the person layer: persons, their identities, the prospects
//! observed about them, and the evidence behind each prospect.
//!
//! Every statement names `workspace_id` on every table it touches (the tenant
//! isolation ratchet counts the ones that do not). Writes go through one
//! transaction per observation, so a prospect never exists without its person
//! and an observation never exists without its prospect.
//!
//! Two rules live here because only the database can enforce them:
//!
//! * **A "no" is not collected against.** A prospect that is `refused` or
//!   `suppressed` receives no further observation and no `last_seen_at` bump —
//!   the sweep that finds the same person again must not quietly keep a file
//!   on someone who has declined.
//! * **Retention has a deadline.** `expires_at` moves forward only on new
//!   evidence (or progress), from the reading's own time; the retention sweep
//!   deletes what passes it without progressing.

use crowdrelay_domain::fan_prospect::{
    ObservationKind, ProspectSource, ProspectStatus, display_handle, normalize_handle,
    normalize_platform,
};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

/// One public signal about one person, as a source read it.
#[derive(Clone, Debug)]
pub struct ObservedPerson<'a> {
    pub source: ProspectSource,
    pub platform: &'a str,
    /// The handle as the platform shows it.
    pub handle: &'a str,
    pub display_name: Option<&'a str>,
    pub profile_url: Option<&'a str>,
    pub kind: ObservationKind,
    /// The row or message the signal came from; with `kind` and `source`, the
    /// idempotency key of the observation.
    pub source_ref: &'a str,
    pub source_url: Option<&'a str>,
    pub observed_at: OffsetDateTime,
    /// Verbatim words from the source. Truncated to the column bound here, so a
    /// long comment cannot make the write fail.
    pub evidence: &'a str,
    pub confidence_basis_points: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserveOutcome {
    /// New prospect (and person) created with its first observation.
    Created { prospect_id: Uuid },
    /// Known prospect; `appended` says whether this was a new observation or a
    /// re-read of one already on file.
    Known { prospect_id: Uuid, appended: bool },
    /// The prospect has said no (or is suppressed). Nothing was written.
    NotCollected { prospect_id: Uuid },
    /// The handle or platform is not an identity. Nothing was written.
    NotAnIdentity,
}

#[derive(Debug, thiserror::Error)]
pub enum ProspectError {
    #[error("fan prospect database operation failed")]
    Database(#[from] sqlx::Error),
}

const EVIDENCE_MAX_CHARS: usize = 500;

fn truncate_evidence(evidence: &str) -> String {
    evidence.trim().chars().take(EVIDENCE_MAX_CHARS).collect()
}

/// Records one observation, creating the person and prospect on first sight.
///
/// # Errors
///
/// Propagates the database error; the transaction rolls back whole.
pub async fn observe(
    pool: &PgPool,
    workspace_id: Uuid,
    seen: &ObservedPerson<'_>,
) -> Result<ObserveOutcome, ProspectError> {
    let (Some(platform), Some(handle)) = (
        normalize_platform(seen.platform),
        normalize_handle(seen.handle),
    ) else {
        return Ok(ObserveOutcome::NotAnIdentity);
    };
    let Some(shown) = display_handle(seen.handle) else {
        return Ok(ObserveOutcome::NotAnIdentity);
    };
    let evidence = truncate_evidence(seen.evidence);
    if evidence.is_empty() {
        return Ok(ObserveOutcome::NotAnIdentity);
    }
    let expires_at = seen.source.expires_at(seen.observed_at);
    let mut tx = pool.begin().await?;

    // The person: found by handle, or created. The unique index on
    // (workspace, kind, platform, value) is the arbiter under a race.
    let existing_person = sqlx::query_scalar::<_, Uuid>(
        "SELECT person_id FROM person_identities
         WHERE workspace_id = $1 AND kind = 'platform_handle'
           AND platform = $2 AND value = $3",
    )
    .bind(workspace_id)
    .bind(&platform)
    .bind(&handle)
    .fetch_optional(&mut *tx)
    .await?;
    let person_id = match existing_person {
        Some(person_id) => person_id,
        None => {
            let person_id = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO persons (workspace_id) VALUES ($1) RETURNING id",
            )
            .bind(workspace_id)
            .fetch_one(&mut *tx)
            .await?;
            let claimed = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO person_identities (workspace_id, person_id, kind, platform, value, source)
                 VALUES ($1, $2, 'platform_handle', $3, $4, $5)
                 ON CONFLICT (workspace_id, kind, COALESCE(platform, ''), value) DO NOTHING
                 RETURNING person_id",
            )
            .bind(workspace_id)
            .bind(person_id)
            .bind(&platform)
            .bind(&handle)
            .bind(seen.source.as_str())
            .fetch_optional(&mut *tx)
            .await?;
            if claimed.is_some() {
                person_id
            } else {
                // Lost a race: another writer claimed the handle first. Drop the
                // person we just made and use theirs.
                sqlx::query("DELETE FROM persons WHERE workspace_id = $1 AND id = $2")
                    .bind(workspace_id)
                    .bind(person_id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT person_id FROM person_identities
                     WHERE workspace_id = $1 AND kind = 'platform_handle'
                       AND platform = $2 AND value = $3",
                )
                .bind(workspace_id)
                .bind(&platform)
                .bind(&handle)
                .fetch_one(&mut *tx)
                .await?
            }
        }
    };

    let prior = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, status FROM fan_prospects
         WHERE workspace_id = $1 AND platform = $2
           AND lower(btrim(external_identity)) = $3
         FOR UPDATE",
    )
    .bind(workspace_id)
    .bind(&platform)
    .bind(&handle)
    .fetch_optional(&mut *tx)
    .await?;

    let (prospect_id, created) = match prior {
        Some((prospect_id, status)) => {
            if ProspectStatus::parse(&status).is_some_and(ProspectStatus::forbids_contact) {
                tx.rollback().await?;
                return Ok(ObserveOutcome::NotCollected { prospect_id });
            }
            sqlx::query(
                "UPDATE fan_prospects
                 SET last_seen_at = GREATEST(last_seen_at, $3),
                     expires_at = GREATEST(expires_at, $4),
                     display_name = COALESCE($5, display_name),
                     profile_url = COALESCE($6, profile_url),
                     updated_at = now()
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id)
            .bind(prospect_id)
            .bind(seen.observed_at)
            .bind(expires_at)
            .bind(seen.display_name)
            .bind(seen.profile_url)
            .execute(&mut *tx)
            .await?;
            (prospect_id, false)
        }
        None => {
            let prospect_id = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO fan_prospects
                     (workspace_id, person_id, platform, external_identity, display_name,
                      profile_url, lawful_basis, expires_at, first_seen_at, last_seen_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9)
                 RETURNING id",
            )
            .bind(workspace_id)
            .bind(person_id)
            .bind(&platform)
            .bind(&shown)
            .bind(seen.display_name)
            .bind(seen.profile_url)
            .bind(seen.source.lawful_basis().as_str())
            .bind(expires_at)
            .bind(seen.observed_at)
            .fetch_one(&mut *tx)
            .await?;
            (prospect_id, true)
        }
    };

    let appended = sqlx::query_scalar::<_, i64>(
        "INSERT INTO fan_prospect_observations
             (workspace_id, prospect_id, observation_kind, source, source_ref, source_url,
              observed_at, evidence, confidence_basis_points)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (prospect_id, observation_kind, source, source_ref) DO NOTHING
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(prospect_id)
    .bind(seen.kind.as_str())
    .bind(seen.source.as_str())
    .bind(seen.source_ref)
    .bind(seen.source_url)
    .bind(seen.observed_at)
    .bind(&evidence)
    .bind(i16::try_from(seen.confidence_basis_points.min(10_000)).unwrap_or(10_000))
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    tx.commit().await?;
    Ok(if created {
        ObserveOutcome::Created { prospect_id }
    } else {
        ObserveOutcome::Known {
            prospect_id,
            appended,
        }
    })
}

/// Prospects per status, for the person-funnel readout. Every status is
/// present with its count (zero is a real count here: the table exists and was
/// read), and `observations` is how much evidence stands behind them.
#[derive(Debug, FromRow)]
pub struct StatusCount {
    pub status: String,
    pub prospects: i64,
}

/// # Errors
///
/// Propagates the database error.
pub async fn status_counts(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<StatusCount>, ProspectError> {
    Ok(sqlx::query_as::<_, StatusCount>(
        "SELECT status, count(*)::bigint AS prospects
         FROM fan_prospects WHERE workspace_id = $1
         GROUP BY status ORDER BY status",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, FromRow)]
pub struct SourceCount {
    pub platform: String,
    pub source: String,
    pub prospects: i64,
    pub converted: i64,
}

/// Prospects and conversions per (platform, source class) — the join between
/// where a person was found and whether they became a fan.
///
/// # Errors
///
/// Propagates the database error.
pub async fn source_counts(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<SourceCount>, ProspectError> {
    Ok(sqlx::query_as::<_, SourceCount>(
        "SELECT p.platform,
                o.source,
                count(DISTINCT p.id)::bigint AS prospects,
                count(DISTINCT p.id) FILTER (WHERE p.status = 'converted')::bigint AS converted
         FROM fan_prospects p
         JOIN fan_prospect_observations o
           ON o.workspace_id = p.workspace_id AND o.prospect_id = p.id
         WHERE p.workspace_id = $1
         GROUP BY p.platform, o.source
         ORDER BY prospects DESC, p.platform, o.source",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?)
}

/// Deletes prospects whose retention has lapsed without progress, and any
/// person left with no prospect. Returns how many prospects were deleted.
/// Converted, refused and suppressed prospects are kept: the first is a fan's
/// provenance, the other two are the record that someone said no.
///
/// # Errors
///
/// Propagates the database error.
pub async fn expire(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    limit: i64,
) -> Result<u64, ProspectError> {
    let mut tx = pool.begin().await?;
    let expired = sqlx::query_as::<_, (Uuid, Uuid)>(
        "DELETE FROM fan_prospects
         WHERE workspace_id = $1
           AND id IN (SELECT id FROM fan_prospects
                      WHERE workspace_id = $1
                        AND expires_at <= $2
                        AND status NOT IN ('converted', 'refused', 'suppressed')
                      ORDER BY expires_at
                      LIMIT $3)
         RETURNING id, person_id",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let people: Vec<Uuid> = expired.iter().map(|(_, person)| *person).collect();
    if !people.is_empty() {
        sqlx::query(
            "DELETE FROM persons
             WHERE workspace_id = $1 AND id = ANY($2)
               AND NOT EXISTS (SELECT 1 FROM fan_prospects p
                               WHERE p.workspace_id = persons.workspace_id
                                 AND p.person_id = persons.id)",
        )
        .bind(workspace_id)
        .bind(&people)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(u64::try_from(expired.len()).unwrap_or(u64::MAX))
}
