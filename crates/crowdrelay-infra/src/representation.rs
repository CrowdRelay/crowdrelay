//! The band-facing half of representation: who it may approach, and the
//! request that queues the approach.
//!
//! The dispatch half lives at the bottom of this file —
//! `lock_representation_for_execution` re-runs the same domain gate when the
//! queued action fires, so nothing written here is trusted past the moment
//! it was checked.
//!
//! Two properties matter and are easy to lose:
//!
//! **The address stays hidden.** `list_targets` never selects
//! `contact_email`. The band sees who the contact is and whether it may
//! approach; the mailbox itself only enters the emitted event, after the
//! gates, inside the transaction that sends it.
//!
//! **A request is a decision, not a send.** `request_approach` writes a
//! decision + an `awaiting_approval` action, the same way an evaluator
//! proposal would land — a person approves it, and dispatch re-checks
//! everything. The gate runs here anyway so a request that cannot send
//! fails on the form, not in the queue.

use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::OutreachTargetId;
use crowdrelay_domain::outreach::OutreachTargetKind;
use crowdrelay_domain::representation::{
    ApproachRequest, MONTHLY_APPROACH_ALLOWANCE, review_approach,
};
use crowdrelay_domain::trace::TraceContext;
use serde::Serialize;
use serde_json::json;
use sqlx::{PgPool, Row};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum RepresentationError {
    #[error("representation database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("no such contact")]
    NotFound,
    /// The domain gate refused the approach. Carries the band-facing
    /// sentence because the caller's job is to show it, not translate it.
    #[error("{0}")]
    Refused(String),
}

/// A representation contact as the band sees it: who it is, what it is,
/// and the consent state. No `contact_email` — the address is the asset
/// the platform brokers, and the band never needs to see it to decide.
#[derive(Clone, Debug, Serialize)]
pub struct RepresentationTarget {
    pub target_id: Uuid,
    /// `agent` or `label`.
    pub kind: String,
    pub display_name: String,
    pub accepts_outreach: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepts_outreach_basis: Option<String>,
    pub do_not_contact: bool,
    pub active: bool,
    pub verified: bool,
    /// Optimistic-concurrency token the approach request pins to.
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_outreach_at: Option<OffsetDateTime>,
}

/// What `request_approach` did. `Replayed` means the same idempotency key
/// already queued an approach — the caller gets the existing action id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApproachOutcome {
    Queued { action_id: Uuid },
    Replayed { action_id: Uuid },
}

#[derive(Clone)]
pub struct PostgresRepresentationRepository {
    pool: PgPool,
}

impl PostgresRepresentationRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The band's representation contacts — every agent and label on its
    /// outreach list, in the order the band should think about them
    /// (contactable and recently untouched first).
    pub async fn list_targets(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<RepresentationTarget>, RepresentationError> {
        let rows = sqlx::query(
            r#"
            SELECT id, target_kind, display_name, accepts_outreach,
                   accepts_outreach_basis, do_not_contact, active, verified,
                   version, last_outreach_at
            FROM viryaos_outreach_targets
            WHERE workspace_id = $1 AND target_kind IN ('agent','label')
            ORDER BY do_not_contact, NOT active, last_outreach_at ASC NULLS FIRST,
                     display_name ASC
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| RepresentationTarget {
                target_id: row.get("id"),
                kind: row.get("target_kind"),
                display_name: row.get("display_name"),
                accepts_outreach: row.get("accepts_outreach"),
                accepts_outreach_basis: row.get("accepts_outreach_basis"),
                do_not_contact: row.get("do_not_contact"),
                active: row.get("active"),
                verified: row.get("verified"),
                version: row.get("version"),
                last_outreach_at: row.get("last_outreach_at"),
            })
            .collect())
    }

    /// Approaches sent this calendar month — what the allowance meter
    /// shows, and what the gate counts.
    pub async fn approaches_used_this_month(
        &self,
        workspace_id: Uuid,
    ) -> Result<u32, RepresentationError> {
        let used = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*) FROM viryaos_outreach_interactions
            WHERE workspace_id = $1 AND phase = 'approach'
              AND occurred_at >= date_trunc('month', now())
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(u32::try_from(used).unwrap_or(u32::MAX))
    }

    /// Queues a band-initiated approach as an `awaiting_approval` action.
    ///
    /// The domain gate runs first under the workspace advisory lock — the
    /// same lock dispatch takes — so a request that would be refused at
    /// send time is refused here, with the band-facing reason. The insert
    /// is idempotent on `idempotency_key`: a retried form submit returns
    /// the action it already queued.
    ///
    /// # Errors
    ///
    /// `Refused` carries the gate's sentence. `NotFound` means the target
    /// id is not a representation contact of this workspace.
    pub async fn request_approach(
        &self,
        workspace_id: Uuid,
        target_id: Uuid,
        note: Option<&str>,
        idempotency_key: &IdempotencyKey,
    ) -> Result<ApproachOutcome, RepresentationError> {
        let mut tx = self.pool.begin().await?;
        let now = OffsetDateTime::now_utc();

        // Same serialization point as dispatch: approach requests and
        // approach sends count the same rows under the same lock.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
            .bind(workspace_id)
            .execute(&mut *tx)
            .await?;

        let target = sqlx::query(
            r#"
            SELECT display_name, target_kind, accepts_outreach,
                   accepts_outreach_basis, do_not_contact, active, verified,
                   version
            FROM viryaos_outreach_targets
            WHERE workspace_id = $1 AND id = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(target_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(RepresentationError::NotFound)?;

        let kind = OutreachTargetKind::parse(&target.get::<String, _>("target_kind"))
            .ok_or(RepresentationError::NotFound)?;
        if !kind.is_representation() {
            return Err(RepresentationError::NotFound);
        }

        let listing_published = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM viryaos_band_listings WHERE workspace_id = $1 AND visibility = 'admitted_readers')",
        )
        .bind(workspace_id)
        .fetch_one(&mut *tx)
        .await?;

        let used = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*) FROM viryaos_outreach_interactions
            WHERE workspace_id = $1 AND phase = 'approach'
              AND occurred_at >= date_trunc('month', $2::timestamptz)
            "#,
        )
        .bind(workspace_id)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;

        // An already-queued approach to the same contact refuses here. The
        // advisory lock above is what makes this check authoritative: two
        // requests serialize on it, so the second one sees the first's row.
        let already_pending = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM viryaos_autopilot_actions
                WHERE workspace_id = $1 AND context = 'representation'
                  AND action_kind = 'representation.approach.request'
                  AND subject_id = $2
                  AND status IN ('awaiting_approval','queued','processing')
            )
            "#,
        )
        .bind(workspace_id)
        .bind(target_id)
        .fetch_one(&mut *tx)
        .await?;

        let trimmed_note = note.map(str::trim).filter(|value| !value.is_empty());
        if let Some(value) = trimmed_note
            && value.chars().count() > 280
        {
            return Err(RepresentationError::Refused(
                "the note rides under the listing — 280 characters is plenty for one line of the band's own words".to_owned(),
            ));
        }

        review_approach(&ApproachRequest {
            accepts_outreach: target.get("accepts_outreach"),
            has_acceptance_basis: target
                .get::<Option<String>, _>("accepts_outreach_basis")
                .is_some(),
            do_not_contact: target.get("do_not_contact"),
            active: target.get("active"),
            verified: target.get("verified"),
            listing_published,
            approaches_used_this_month: u32::try_from(used).unwrap_or(u32::MAX)
                + u32::from(already_pending),
            allowance: MONTHLY_APPROACH_ALLOWANCE,
        })
        .map_err(|refusal| RepresentationError::Refused(refusal.message()))?;
        if already_pending {
            return Err(RepresentationError::Refused(
                "an approach to this contact is already waiting on an approve".to_owned(),
            ));
        }

        let payload = crowdrelay_application::autopilot::AutopilotActionPayload::RequestRepresentationApproach {
            target_id: OutreachTargetId::from_uuid(target_id),
            target_version: target.get("version"),
            target_name: target.get("display_name"),
            note: trimmed_note.map(str::to_owned),
        };
        let action_kind = payload.action_kind();
        let action_class = payload.action_class().as_str();
        let payload_json = serde_json::to_value(&payload).map_err(|_| {
            RepresentationError::Refused("the approach could not be encoded".to_owned())
        })?;

        // A replay finds the action already queued under this key and
        // returns it — no second decision, no second row.
        if let Some(existing) = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM viryaos_autopilot_actions WHERE workspace_id = $1 AND idempotency_key = $2",
        )
        .bind(workspace_id)
        .bind(idempotency_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(ApproachOutcome::Replayed { action_id: existing });
        }

        let trace = TraceContext::root(crowdrelay_domain::WorkspaceId::from_uuid(workspace_id));
        let decision_key = format!("representation.approach:{}", idempotency_key.as_str());
        let decision_id = match sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO viryaos_autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
            ) VALUES ($1,$2,$3,'representation','outreach_target',$4,
                      'representation.approach',10000,'require_approval',
                      'Band-initiated representation approach',
                      $5,$6,$7,$8,$9)
            ON CONFLICT (workspace_id, decision_key) DO NOTHING RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(&decision_key)
        .bind(target_id)
        .bind(json!({
            "target_id": target_id,
            "note": trimmed_note,
            "listing_published": listing_published,
            "approaches_used_this_month": used,
        }))
        .bind(json!({"require_approval": true, "monthly_allowance": MONTHLY_APPROACH_ALLOWANCE}))
        .bind(payload_json.clone())
        .bind(now)
        .bind(trace.trace_id().into_uuid())
        .fetch_optional(&mut *tx)
        .await?
        {
            Some(id) => id,
            // A decision under this key already exists — the action lookup
            // above found no matching action, so a prior attempt died
            // between the two inserts. Reuse the decision and queue the
            // action it was meant to carry.
            None => sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM viryaos_autopilot_decisions WHERE workspace_id = $1 AND decision_key = $2",
            )
            .bind(workspace_id)
            .bind(&decision_key)
            .fetch_one(&mut *tx)
            .await?,
        };
        let action_id = Uuid::now_v7();
        let action_trace = TraceContext::for_action(
            crowdrelay_domain::WorkspaceId::from_uuid(workspace_id),
            trace.trace_id(),
            action_id,
            Some(decision_id),
        );
        sqlx::query(
            r#"
            INSERT INTO viryaos_autopilot_actions (
                id, workspace_id, decision_id, context, action_kind,
                subject_kind, subject_id, idempotency_key, payload, status,
                action_class, approval_expires_at, trace_id, causation_id
            ) VALUES ($1,$2,$3,'representation',$4,'outreach_target',$5,$6,$7,
                      'awaiting_approval',$8, now() + INTERVAL '72 hours',
                      $9,$10)
            "#,
        )
        .bind(action_id)
        .bind(workspace_id)
        .bind(decision_id)
        .bind(action_kind)
        .bind(target_id)
        .bind(idempotency_key.as_str())
        .bind(payload_json)
        .bind(action_class)
        .bind(action_trace.trace_id().into_uuid())
        .bind(action_trace.causation_id().map(|c| c.into_uuid()))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(ApproachOutcome::Queued { action_id })
    }
}

// ── The dispatch half ────────────────────────────────────────────────
//
// Called from `autopilot::actions_execution` when a queued approach action
// fires. Both functions run inside the dispatch transaction; they return the
// application's `RepositoryError` because the dispatcher's failure ladder
// reads it, not because this layer owns it.

fn map_sqlx(error: sqlx::Error) -> crowdrelay_application::RepositoryError {
    use crowdrelay_application::RepositoryError as E;
    match crate::database::classify_sqlx_error(&error) {
        crate::database::SqlxErrorClass::NotFound => E::NotFound,
        crate::database::SqlxErrorClass::Conflict => E::Conflict,
        crate::database::SqlxErrorClass::Unavailable => E::Unavailable,
        crate::database::SqlxErrorClass::Unexpected => E::Unexpected,
    }
}

/// Locks a representation contact (agent or label) for an approach and
/// re-runs every request-time gate: consent, verification, the published
/// listing, and the monthly allowance.
///
/// Approaches have no opportunity row — the band names the target directly —
/// so this locks the target itself. The workspace advisory lock serializes
/// concurrent approach dispatches, which is what makes the allowance count
/// honest: two in-flight approaches cannot both read the same headroom.
/// Re-gating matters because the flags can change between approval and
/// dispatch (an operator marks the contact do-not-contact, the band unlists,
/// the month's approaches fill up).
pub(crate) async fn lock_representation_for_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: crowdrelay_domain::WorkspaceId,
    target_id: OutreachTargetId,
    target_version: i64,
    now: OffsetDateTime,
) -> Result<(String, String, String), crowdrelay_application::RepositoryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(workspace_id.into_uuid())
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;

    let target = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            bool,
            Option<String>,
            bool,
            bool,
            bool,
        ),
    >(
        r#"
      SELECT display_name, contact_email, target_kind, accepts_outreach,
             accepts_outreach_basis, do_not_contact, active, verified
      FROM viryaos_outreach_targets
      WHERE workspace_id=$1 AND id=$2 AND version=$3
      FOR UPDATE
    "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id.into_uuid())
    .bind(target_version)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(crowdrelay_application::RepositoryError::Conflict)?;

    let kind = OutreachTargetKind::parse(&target.2)
        .ok_or(crowdrelay_application::RepositoryError::Conflict)?;
    if !kind.is_representation() {
        return Err(crowdrelay_application::RepositoryError::Conflict);
    }

    // FOR UPDATE so a publish/unlist racing this dispatch cannot flip the
    // answer between the gate and the emit inside the same transaction.
    let listing_visibility = sqlx::query_scalar::<_, String>(
        "SELECT visibility FROM viryaos_band_listings WHERE workspace_id=$1 FOR UPDATE",
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    let approaches_used = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*) FROM viryaos_outreach_interactions
        WHERE workspace_id=$1 AND phase='approach'
          AND occurred_at >= date_trunc('month', $2::timestamptz)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;

    crowdrelay_domain::representation::review_approach(
        &crowdrelay_domain::representation::ApproachRequest {
            accepts_outreach: target.3,
            has_acceptance_basis: target.4.is_some(),
            do_not_contact: target.5,
            active: target.6,
            verified: target.7,
            listing_published: listing_visibility.as_deref() == Some("admitted_readers"),
            approaches_used_this_month: u32::try_from(approaches_used).unwrap_or(u32::MAX),
            allowance: MONTHLY_APPROACH_ALLOWANCE,
        },
    )
    .map_err(|_| crowdrelay_application::RepositoryError::Conflict)?;

    Ok((target.0, target.1, target.2))
}

/// Records that an approach went out: the contact's `last_outreach_at`, an
/// `approach` interaction (the row the monthly allowance counts), and the
/// reach-ledger event.
pub(crate) async fn record_approach_sent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: crowdrelay_domain::WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    target_id: OutreachTargetId,
    now: OffsetDateTime,
) -> Result<(), crowdrelay_application::RepositoryError> {
    sqlx::query("UPDATE viryaos_outreach_targets SET last_outreach_at=$3,contact_verified_at=CASE WHEN contact_verified_at IS NULL OR contact_verified_at < $3 THEN $3 ELSE contact_verified_at END WHERE workspace_id=$1 AND id=$2")
      .bind(workspace_id.into_uuid()).bind(target_id.into_uuid()).bind(now).execute(&mut **tx).await.map_err(map_sqlx)?;
    sqlx::query(r#"INSERT INTO viryaos_outreach_interactions(workspace_id,target_id,opportunity_id,direction,phase,source_key,occurred_at) VALUES($1,$2,NULL,'outbound','approach',$3,$4) ON CONFLICT(workspace_id,target_id,source_key) DO NOTHING"#)
      .bind(workspace_id.into_uuid()).bind(target_id.into_uuid()).bind(format!("autopilot:{}",action_id)).bind(now).execute(&mut **tx).await.map_err(map_sqlx)?;
    sqlx::query(r#"INSERT INTO viryaos_reach_events (workspace_id, action_id, recipient_kind, recipient_id, channel, template_id, estimated_reach, status, metadata) VALUES ($1, $2, 'outreach_target', $3::text, 'email', 'representation', 1, 'sent', jsonb_build_object('kind', 'approach')) ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL DO NOTHING"#)
      .bind(workspace_id.into_uuid()).bind(action_id.into_uuid()).bind(target_id.into_uuid()).execute(&mut **tx).await.map_err(map_sqlx)?;
    Ok(())
}
