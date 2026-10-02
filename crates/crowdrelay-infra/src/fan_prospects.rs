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

use crowdrelay_domain::{
    fan_next_action::{
        FanProspectActionInput, FanProspectActionKind, FanProspectCtaIntent,
        FanProspectMedium, evaluate_fan_prospect,
    },
    fan_prospect::{
        ObservationKind, ProspectSource, ProspectStatus, display_handle, normalize_handle,
        normalize_platform, normalize_platform_user_id,
    },
};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

/// One public signal about one person, as a source read it.
#[derive(Clone, Debug)]
pub struct ObservedPerson<'a> {
    pub source: ProspectSource,
    pub platform: &'a str,
    /// Stable provider/user/channel id when the surface exposes one.
    pub platform_user_id: Option<&'a str>,
    /// Public handle when the surface exposes one.
    pub handle: Option<&'a str>,
    /// Human-readable source identity. Matching never relies on this when a
    /// stable provider id exists.
    pub display_identity: &'a str,
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
    /// Neither a stable platform id nor a valid handle was present.
    NotAnIdentity,
    /// Supplied identities already belong to different people. Ingestion
    /// never silently merges them.
    IdentityConflict,
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
    let Some(platform) = normalize_platform(seen.platform) else {
        return Ok(ObserveOutcome::NotAnIdentity);
    };
    let stable_id = seen.platform_user_id.and_then(normalize_platform_user_id);
    let handle = seen.handle.and_then(normalize_handle);
    if stable_id.is_none() && handle.is_none() {
        return Ok(ObserveOutcome::NotAnIdentity);
    }
    let shown = {
        let display = seen.display_identity.trim();
        if !display.is_empty() && display.chars().count() <= 256 {
            display.to_owned()
        } else if let Some(handle) = seen.handle.and_then(display_handle) {
            handle
        } else {
            stable_id.clone().unwrap_or_default()
        }
    };
    if shown.is_empty() {
        return Ok(ObserveOutcome::NotAnIdentity);
    }

    let evidence = truncate_evidence(seen.evidence);
    if evidence.is_empty() {
        return Ok(ObserveOutcome::NotAnIdentity);
    }
    let expires_at = seen.source.expires_at(seen.observed_at);
    let mut tx = pool.begin().await?;

    let stable_person = if let Some(stable_id) = stable_id.as_deref() {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT person_id FROM person_identities
             WHERE workspace_id=$1 AND kind='platform_user_id'
               AND platform=$2 AND value=$3",
        )
        .bind(workspace_id)
        .bind(&platform)
        .bind(stable_id)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        None
    };
    let handle_person = if let Some(handle) = handle.as_deref() {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT person_id FROM person_identities
             WHERE workspace_id=$1 AND kind='platform_handle'
               AND platform=$2 AND value=$3",
        )
        .bind(workspace_id)
        .bind(&platform)
        .bind(handle)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        None
    };
    if stable_person.is_some() && handle_person.is_some() && stable_person != handle_person {
        tx.rollback().await?;
        return Ok(ObserveOutcome::IdentityConflict);
    }

    let person_id = if let Some(person_id) = stable_person.or(handle_person) {
        person_id
    } else {
        let person_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO persons (workspace_id) VALUES ($1) RETURNING id",
        )
        .bind(workspace_id)
        .fetch_one(&mut *tx)
        .await?;
        let (kind, value) = if let Some(stable_id) = stable_id.as_deref() {
            ("platform_user_id", stable_id)
        } else {
            ("platform_handle", handle.as_deref().expect("handle exists"))
        };
        let claimed = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO person_identities
                 (workspace_id, person_id, kind, platform, value, source)
             VALUES ($1,$2,$3,$4,$5,$6)
             ON CONFLICT (workspace_id, kind, COALESCE(platform, ''), value)
             DO NOTHING
             RETURNING person_id",
        )
        .bind(workspace_id)
        .bind(person_id)
        .bind(kind)
        .bind(&platform)
        .bind(value)
        .bind(seen.source.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if claimed.is_some() {
            person_id
        } else {
            sqlx::query("DELETE FROM persons WHERE workspace_id=$1 AND id=$2")
                .bind(workspace_id)
                .bind(person_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query_scalar::<_, Uuid>(
                "SELECT person_id FROM person_identities
                 WHERE workspace_id=$1 AND kind=$2 AND platform=$3 AND value=$4",
            )
            .bind(workspace_id)
            .bind(kind)
            .bind(&platform)
            .bind(value)
            .fetch_one(&mut *tx)
            .await?
        }
    };

    for (kind, value) in [
        stable_id
            .as_deref()
            .map(|value| ("platform_user_id", value)),
        handle.as_deref().map(|value| ("platform_handle", value)),
    ]
    .into_iter()
    .flatten()
    {
        sqlx::query(
            "INSERT INTO person_identities
                 (workspace_id, person_id, kind, platform, value, source)
             VALUES ($1,$2,$3,$4,$5,$6)
             ON CONFLICT (workspace_id, kind, COALESCE(platform, ''), value)
             DO NOTHING",
        )
        .bind(workspace_id)
        .bind(person_id)
        .bind(kind)
        .bind(&platform)
        .bind(value)
        .bind(seen.source.as_str())
        .execute(&mut *tx)
        .await?;
        let owner = sqlx::query_scalar::<_, Uuid>(
            "SELECT person_id FROM person_identities
             WHERE workspace_id=$1 AND kind=$2 AND platform=$3 AND value=$4",
        )
        .bind(workspace_id)
        .bind(kind)
        .bind(&platform)
        .bind(value)
        .fetch_one(&mut *tx)
        .await?;
        if owner != person_id {
            tx.rollback().await?;
            return Ok(ObserveOutcome::IdentityConflict);
        }
    }

    let prior = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, status FROM fan_prospects
         WHERE workspace_id=$1 AND person_id=$2 AND platform=$3
         FOR UPDATE",
    )
    .bind(workspace_id)
    .bind(person_id)
    .bind(&platform)
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
                 SET last_seen_at=GREATEST(last_seen_at,$3),
                     expires_at=GREATEST(expires_at,$4),
                     external_identity=CASE
                         WHEN $3 >= last_seen_at THEN $5
                         ELSE external_identity
                     END,
                     display_name=CASE
                         WHEN $3 >= last_seen_at THEN COALESCE($6,display_name)
                         ELSE display_name
                     END,
                     profile_url=CASE
                         WHEN $3 >= last_seen_at THEN COALESCE($7,profile_url)
                         ELSE profile_url
                     END,
                     updated_at=now()
                 WHERE workspace_id=$1 AND id=$2",
            )
            .bind(workspace_id)
            .bind(prospect_id)
            .bind(seen.observed_at)
            .bind(expires_at)
            .bind(&shown)
            .bind(seen.display_name)
            .bind(seen.profile_url)
            .execute(&mut *tx)
            .await?;
            (prospect_id, false)
        }
        None => {
            let prospect_id = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO fan_prospects
                     (workspace_id,person_id,platform,external_identity,display_name,
                      profile_url,lawful_basis,expires_at,first_seen_at,last_seen_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$9)
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
             (workspace_id,prospect_id,observation_kind,source,source_ref,source_url,
              observed_at,evidence,confidence_basis_points)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (prospect_id,observation_kind,source,source_ref) DO NOTHING
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

/// Links a prospect to an existing verified first-party fan.
///
/// This never creates a fan row. Refused/suppressed prospects are not revived,
/// and a prospect already linked to another fan cannot be silently relinked.
///
/// # Errors
/// Propagates database errors.
pub async fn link_verified_fan(
    pool: &PgPool,
    workspace_id: Uuid,
    prospect_id: Uuid,
    fan_id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, ProspectError> {
    let linked = sqlx::query_scalar::<_, Uuid>(
        "UPDATE fan_prospects AS prospect
         SET linked_fan_id=$3,
             status='converted',
             last_seen_at=GREATEST(prospect.last_seen_at,$4),
             updated_at=$4
         WHERE prospect.workspace_id=$1
           AND prospect.id=$2
           AND prospect.status NOT IN ('refused','suppressed')
           AND (prospect.linked_fan_id IS NULL OR prospect.linked_fan_id=$3)
           AND EXISTS (
               SELECT 1 FROM fans AS fan
               WHERE fan.workspace_id=$1
                 AND fan.id=$3
                 AND fan.status='active'
                 AND fan.deleted_at IS NULL
                 AND EXISTS (
                     SELECT 1 FROM fan_identifiers AS identifier
                     WHERE identifier.workspace_id=fan.workspace_id
                       AND identifier.fan_id=fan.id
                       AND identifier.verified_at IS NOT NULL
                 )
           )
         RETURNING prospect.id",
    )
    .bind(workspace_id)
    .bind(prospect_id)
    .bind(fan_id)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(linked.is_some())
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


const MAX_ACTION_READ_ROWS: i64 = 500;
const MAX_ACTION_VIEW_ROWS: usize = 100;

#[derive(Debug, FromRow)]
struct ProspectActionRow {
    id: Uuid,
    status: String,
    platform: String,
    external_identity: String,
    last_seen_at: OffsetDateTime,
    observation_count: i64,
    same_thread_context: bool,
    explicit_join_or_follow_intent: bool,
    question_intent: bool,
    warm_engagement: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct ProspectActionView {
    pub prospect_id: Uuid,
    pub platform: String,
    pub external_identity: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    pub observation_count: i64,
    pub action: FanProspectActionKind,
    pub medium: Option<FanProspectMedium>,
    pub cta_intent: Option<FanProspectCtaIntent>,
    pub reason: &'static str,
    pub cooldown_hours: Option<u32>,
    pub measurement: &'static str,
}

/// Read-only relationship decisions for the current prospect pool.
///
/// The identity appears only on the private control-plane surface that consumes
/// this view. No action is persisted and no authority is granted here.
///
/// # Errors
/// Database failure.
pub async fn next_actions(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Vec<ProspectActionView>, ProspectError> {
    let member_site_ready = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM tenant_settings
             WHERE workspace_id=$1
               AND key='member_site_base_url'
               AND btrim(value) <> ''
         )",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query_as::<_, ProspectActionRow>(
        r#"
        SELECT p.id, p.status, p.platform, p.external_identity, p.last_seen_at,
               count(o.id)::bigint AS observation_count,
               COALESCE(bool_or(o.source = 'own_comments'), false) AS same_thread_context,
               COALESCE(bool_or(o.observation_kind = 'asked_to_join_or_follow'), false)
                   AS explicit_join_or_follow_intent,
               COALESCE(bool_or(o.observation_kind IN ('asked_about_show','asked_for_music')), false)
                   AS question_intent,
               COALESCE(bool_or(o.observation_kind IN (
                   'active_under_our_post','replied','shared_material'
               )), false) AS warm_engagement
        FROM fan_prospects p
        LEFT JOIN fan_prospect_observations o
          ON o.workspace_id=p.workspace_id AND o.prospect_id=p.id
        WHERE p.workspace_id=$1
        GROUP BY p.id, p.status, p.platform, p.external_identity, p.last_seen_at
        ORDER BY p.last_seen_at DESC, p.id
        LIMIT $2
        "#,
    )
    .bind(workspace_id)
    .bind(MAX_ACTION_READ_ROWS)
    .fetch_all(pool)
    .await?;

    let mut views = Vec::with_capacity(rows.len().min(MAX_ACTION_VIEW_ROWS));
    for row in rows {
        let Some(status) = ProspectStatus::parse(&row.status) else {
            continue;
        };
        let decision = evaluate_fan_prospect(FanProspectActionInput {
            status,
            has_same_thread_context: row.same_thread_context,
            explicit_join_or_follow_intent: row.explicit_join_or_follow_intent,
            question_intent: row.question_intent,
            warm_engagement: row.warm_engagement,
            member_site_ready,
        });
        views.push(ProspectActionView {
            prospect_id: row.id,
            platform: row.platform,
            external_identity: row.external_identity,
            status: row.status,
            last_seen_at: row.last_seen_at,
            observation_count: row.observation_count,
            action: decision.action,
            medium: decision.medium,
            cta_intent: decision.cta_intent,
            reason: decision.reason,
            cooldown_hours: decision.cooldown_hours,
            measurement: decision.measurement,
        });
    }
    views.sort_by(|left, right| {
        right
            .action
            .priority()
            .cmp(&left.action.priority())
            .then_with(|| right.last_seen_at.cmp(&left.last_seen_at))
            .then_with(|| left.prospect_id.cmp(&right.prospect_id))
    });
    views.truncate(MAX_ACTION_VIEW_ROWS);
    Ok(views)
}
