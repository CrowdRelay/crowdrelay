//! The booker's answer to a Beacon recommendation, written where the next
//! cycle reads it.
//!
//! Defer and decline are the two "no" a review needs to keep. Without them a
//! cancelled ask is a missing row — the snapshot loader reads nothing, the
//! detector offers the same pair again next cycle, and the queue teaches the
//! booker that saying no sticks to nothing.
//!
//! Both verbs upsert the (beacon, event) campaign row — the recommendation
//! may precede any outreach, so the row often does not exist yet — and cancel
//! every ask still waiting in the approval queue for that pair, in one
//! transaction. A campaign already `partner` or `closed` keeps its standing:
//! declining tonight's ask does not demote a partner who already said yes.
//!
//! `deferred_until` is bounded by the event itself in the snapshot loader —
//! a stale defer on a passed show cannot suppress next year's ask.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{OperatorActionRecord, record_operator_action};

/// Beacon action kinds an operator answer must pull out of the approval
/// queue: the named outreach ask and the community invite batch.
const PAIR_ACTION_KINDS: &[&str] = &["beacon.outreach.request", "beacon.invite_batch.request"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutreachStateChanged {
    pub beacon_id: Uuid,
    pub event_id: Uuid,
    pub status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub deferred_until: Option<OffsetDateTime>,
    /// Pending approval-queue asks cancelled by this answer.
    pub cancelled_actions: i64,
    /// True when the campaign row kept a stronger state (`partner`,
    /// `closed`): the answer is recorded on the audit ledger but does not
    /// demote a relationship that already went further.
    pub preserved_stronger_state: bool,
    /// True when the idempotency key was already consumed by this same
    /// answer on this same pair — the stored outcome is replayed rather
    /// than a second transition applied.
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutreachStateRefusal {
    /// No active beacon row for this workspace.
    BeaconNotFound,
    /// No such event in the workspace.
    EventNotFound,
    /// `days` outside the accepted range.
    BadDeferDays,
    /// The idempotency key was already consumed — either a true replay the
    /// caller can treat as done, or a key reused against a different pair,
    /// which the ledger refuses to merge silently.
    IdempotencyConflict,
}

/// Both verbs need the pair to exist before the answer means anything: a
/// defer or decline on a beacon or event the workspace does not have is a
/// typo, not a decision.
async fn check_pair(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    beacon_id: Uuid,
    event_id: Uuid,
) -> Result<(), OutreachStateRefusal> {
    let beacon: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM beacons WHERE workspace_id = $1 AND id = $2 AND active)",
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|_| OutreachStateRefusal::BeaconNotFound)?;
    if !beacon {
        return Err(OutreachStateRefusal::BeaconNotFound);
    }
    let event: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM events WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|_| OutreachStateRefusal::EventNotFound)?;
    if !event {
        return Err(OutreachStateRefusal::EventNotFound);
    }
    Ok(())
}

/// Cancel every approval-queue ask for the pair. Queued or running sends are
/// already past the booker's decision point — the executor re-reads the
/// campaign state before sending, so an answer written here still gates the
/// send itself.
async fn cancel_pending_asks(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    beacon_id: Uuid,
    event_id: Uuid,
) -> Result<i64, sqlx::Error> {
    let cancelled = sqlx::query(
        r#"
        UPDATE autopilot_actions
        SET status = 'cancelled', finished_at = now(),
            -- Beacon asks carry a deterministic key; a cancelled row holding
            -- it would swallow every later mint (`ON CONFLICT DO NOTHING`)
            -- and a lapsed defer could never re-raise. Suffix the dead row
            -- so the base key is free when the pair is due again.
            idempotency_key = idempotency_key || ':operator:' || id::text
        WHERE workspace_id = $1
          AND status = 'awaiting_approval'
          AND action_kind = ANY($2)
          AND payload->>'beacon_id' = $3::text
          AND payload->>'event_id' = $4::text
        "#,
    )
    .bind(workspace_id)
    .bind(PAIR_ACTION_KINDS)
    .bind(beacon_id.to_string())
    .bind(event_id.to_string())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    #[allow(clippy::cast_possible_wrap)]
    Ok(cancelled as i64)
}

/// Stamp the operator's decline on the pair ledger from inside an existing
/// transaction — the approval-queue cancel path uses this so a cancelled
/// ask cannot re-appear next cycle as a missing row. Stronger states
/// (`partner`, `closed`, `suppressed`) keep theirs. The cancelled action's
/// deterministic key is released so a pair the partner later warms to can
/// mint the same ask again.
pub(crate) async fn stamp_operator_decline(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    beacon_id: Uuid,
    event_id: Uuid,
    cancelled_action_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO beacon_campaigns (
            workspace_id, beacon_id, event_id, status, declined_via
        ) VALUES ($1, $2, $3, 'declined', 'operator')
        ON CONFLICT (workspace_id, beacon_id, event_id) DO UPDATE SET
            status = 'declined',
            -- The partner's own "no" is stronger provenance than the
            -- booker's bookkeeping; a later operator answer never
            -- rewrites who declined.
            declined_via = CASE
                WHEN beacon_campaigns.declined_via = 'partner_reply'
                THEN 'partner_reply'
                ELSE 'operator'
            END,
            deferred_until = NULL
        WHERE beacon_campaigns.status NOT IN ('partner','closed','suppressed')
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .execute(&mut **tx)
    .await?;
    if let Some(action_id) = cancelled_action_id {
        // A cancelled row that keeps the deterministic key swallows every
        // later mint (`ON CONFLICT DO NOTHING`).
        sqlx::query(
            "UPDATE autopilot_actions \
             SET idempotency_key = idempotency_key || ':operator:' || id::text \
             WHERE workspace_id = $1 AND id = $2 AND status = 'cancelled'",
        )
        .bind(workspace_id)
        .bind(action_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Probe the audit ledger for a consumed key, inside the transaction that
/// is about to write the answer. The same key on the same verb and pair
/// replays the stored outcome without a second transition; the same key on
/// a different verb or pair is a reuse the ledger refuses to merge.
enum KeyProbe {
    Free,
    Replayed(OutreachStateChanged),
    Conflict,
}

async fn probe_operator_key(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    action: &str,
    beacon_id: Uuid,
    event_id: Uuid,
    idempotency_key: &str,
) -> Result<KeyProbe, sqlx::Error> {
    let existing = sqlx::query_as::<_, (String, Uuid, Option<String>)>(
        "SELECT action, target_id, details->>'event_id' \
         FROM operator_actions WHERE workspace_id = $1 AND idempotency_key = $2",
    )
    .bind(workspace_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((recorded_action, target_id, recorded_event)) = existing else {
        return Ok(KeyProbe::Free);
    };
    if recorded_action != action
        || target_id != beacon_id
        || recorded_event.as_deref() != Some(event_id.to_string().as_str())
    {
        return Ok(KeyProbe::Conflict);
    }
    // Replay answers with the pair's current ledger state instead of
    // re-applying the transition — a re-sent defer must not stretch the
    // window by another `days`.
    let row = sqlx::query_as::<_, (String, Option<OffsetDateTime>)>(
        "SELECT status, deferred_until FROM beacon_campaigns \
         WHERE workspace_id = $1 AND beacon_id = $2 AND event_id = $3",
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .fetch_optional(&mut **tx)
    .await?;
    let (status, deferred_until) = row.unwrap_or_else(|| ("candidate".to_owned(), None));
    let preserved = matches!(
        status.as_str(),
        "declined" | "suppressed" | "partner" | "closed"
    );
    Ok(KeyProbe::Replayed(OutreachStateChanged {
        beacon_id,
        event_id,
        status,
        deferred_until,
        cancelled_actions: 0,
        preserved_stronger_state: preserved,
        replayed: true,
    }))
}

/// `defer`: hold the pair out of the due set until `now + days`, without
/// declining it. `days` is operator-chosen within 1..=90 — whether that
/// outlives the show is the loader's bound, not this write's.
pub async fn defer_beacon_outreach(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    event_id: Uuid,
    days: i32,
    idempotency_key: &str,
    request_id: Option<&str>,
) -> Result<Result<OutreachStateChanged, OutreachStateRefusal>, sqlx::Error> {
    if !(1..=90).contains(&days) {
        return Ok(Err(OutreachStateRefusal::BadDeferDays));
    }
    let mut tx = pool.begin().await?;
    match probe_operator_key(
        &mut tx,
        workspace_id,
        "beacon_outreach_defer",
        beacon_id,
        event_id,
        idempotency_key,
    )
    .await?
    {
        KeyProbe::Replayed(changed) => {
            tx.commit().await?;
            return Ok(Ok(changed));
        }
        KeyProbe::Conflict => return Ok(Err(OutreachStateRefusal::IdempotencyConflict)),
        KeyProbe::Free => {}
    }
    if let Err(refusal) = check_pair(&mut tx, workspace_id, beacon_id, event_id).await {
        return Ok(Err(refusal));
    }

    let deferred_until = OffsetDateTime::now_utc() + time::Duration::days(i64::from(days));
    // Defer does not lower a stronger state: a declined pair stays declined,
    // a partner stays a partner — and neither gets a `deferred_until` that
    // would lie about what the answer was. The stamp only lands while the
    // pair is still in play.
    let row = sqlx::query_as::<_, (String, Option<OffsetDateTime>)>(
        r#"
        INSERT INTO beacon_campaigns (workspace_id, beacon_id, event_id, status, deferred_until)
        VALUES ($1, $2, $3, 'candidate', $4)
        ON CONFLICT (workspace_id, beacon_id, event_id) DO UPDATE SET
            deferred_until = CASE
                WHEN beacon_campaigns.status IN ('declined','suppressed','partner','closed')
                THEN beacon_campaigns.deferred_until
                ELSE $4
            END
        RETURNING status, deferred_until
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .bind(deferred_until)
    .fetch_one(&mut *tx)
    .await?;

    let cancelled_actions = cancel_pending_asks(&mut tx, workspace_id, beacon_id, event_id).await?;

    let recorded = record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "beacon_outreach_defer",
            target_type: "beacon",
            target_id: beacon_id,
            idempotency_key,
            request_id,
            details: serde_json::json!({
                "event_id": event_id,
                "deferred_until": deferred_until,
                "days": days,
                "cancelled_actions": cancelled_actions,
            }),
        },
    )
    .await?;
    if !recorded {
        return Ok(Err(OutreachStateRefusal::IdempotencyConflict));
    }

    tx.commit().await?;
    let preserved = matches!(
        row.0.as_str(),
        "declined" | "suppressed" | "partner" | "closed"
    );
    Ok(Ok(OutreachStateChanged {
        beacon_id,
        event_id,
        status: row.0,
        deferred_until: row.1,
        cancelled_actions,
        preserved_stronger_state: preserved,
        replayed: false,
    }))
}

/// `decline`: the operator's "not this pair", as durable as a partner's "no".
/// The due set stops offering the ask; a later fresh evaluation can still
/// re-open the pair only through a new candidate that re-qualifies on its
/// own evidence.
pub async fn decline_beacon_outreach(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    event_id: Uuid,
    reason: Option<&str>,
    idempotency_key: &str,
    request_id: Option<&str>,
) -> Result<Result<OutreachStateChanged, OutreachStateRefusal>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    match probe_operator_key(
        &mut tx,
        workspace_id,
        "beacon_outreach_decline",
        beacon_id,
        event_id,
        idempotency_key,
    )
    .await?
    {
        KeyProbe::Replayed(changed) => {
            tx.commit().await?;
            return Ok(Ok(changed));
        }
        KeyProbe::Conflict => return Ok(Err(OutreachStateRefusal::IdempotencyConflict)),
        KeyProbe::Free => {}
    }
    if let Err(refusal) = check_pair(&mut tx, workspace_id, beacon_id, event_id).await {
        return Ok(Err(refusal));
    }

    let status = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO beacon_campaigns (
            workspace_id, beacon_id, event_id, status, declined_via, notes
        ) VALUES ($1, $2, $3, 'declined', 'operator', $4)
        ON CONFLICT (workspace_id, beacon_id, event_id) DO UPDATE SET
            status = CASE
                WHEN beacon_campaigns.status IN ('partner','closed','suppressed')
                THEN beacon_campaigns.status
                ELSE 'declined'
            END,
            declined_via = CASE
                WHEN beacon_campaigns.status IN ('partner','closed','suppressed')
                THEN beacon_campaigns.declined_via
                WHEN beacon_campaigns.declined_via = 'partner_reply'
                THEN 'partner_reply'
                ELSE 'operator'
            END,
            notes = COALESCE($4, beacon_campaigns.notes),
            deferred_until = NULL
        RETURNING status
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .bind(reason.map(str::trim).filter(|r| !r.is_empty()).map(|r| {
        if r.chars().count() > 2000 {
            r.chars().take(2000).collect::<String>()
        } else {
            r.to_owned()
        }
    }))
    .fetch_one(&mut *tx)
    .await?;

    let cancelled_actions = cancel_pending_asks(&mut tx, workspace_id, beacon_id, event_id).await?;

    let recorded = record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "beacon_outreach_decline",
            target_type: "beacon",
            target_id: beacon_id,
            idempotency_key,
            request_id,
            details: serde_json::json!({
                "event_id": event_id,
                "reason": reason,
                "cancelled_actions": cancelled_actions,
                "resulting_status": status,
            }),
        },
    )
    .await?;
    if !recorded {
        return Ok(Err(OutreachStateRefusal::IdempotencyConflict));
    }

    tx.commit().await?;
    let preserved = matches!(status.as_str(), "partner" | "closed" | "suppressed");
    Ok(Ok(OutreachStateChanged {
        beacon_id,
        event_id,
        status,
        deferred_until: None,
        cancelled_actions,
        preserved_stronger_state: preserved,
        replayed: false,
    }))
}
