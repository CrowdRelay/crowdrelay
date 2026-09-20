//! What died in the approval queue (N.13).
//!
//! The operator surface has always shown what is *pending*: `needs_you` lists
//! every action awaiting approval and carries its `approval_expires_at`, so a
//! person looking at the queue can see a deadline. Nothing has ever shown the
//! other half — the asks that reached that deadline and were reaped.
//!
//! Three ways an ask dies, and they are kept apart because the operator's next
//! step differs for each:
//!
//! * `approval_expired` — the window closed with nobody answering. The work the
//!   machine did is spent: a letter nobody sent, an opportunity nobody took. The
//!   remedy is the queue's throughput, which is a person.
//! * `insufficient_evidence` — the machine withdrew its own ask, because the
//!   decision behind it carried zero confidence. Nothing was lost and no
//!   operator failed to act; showing it as a lapse would teach the queue's
//!   reader to distrust their own diligence.
//! * `awaiting_sweep` — past its deadline and still sitting in
//!   `awaiting_approval`, because no sweep has run for this workspace yet.
//!   The claim path sweeps on every autopilot cycle and the retention worker
//!   sweeps globally every hour, so a nonzero population here means neither
//!   reached it — a parked or disabled tenant, or a stalled worker (the
//!   `approval.sweep_lagging` watchdog watches exactly this). `load_needs_you`
//!   filters these out with `approval_expires_at > now()`, so without this
//!   read they are invisible on both sides: too late to be pending, not yet
//!   cancelled.
//!
//! Why it matters more here than the numbers suggest: every outbound channel in
//! this system drafts and waits for a person, so the operator *is* the
//! throughput limit. A queue that empties itself every 72 hours without telling
//! anybody converts that limit into silent loss — the most expensive kind,
//! because nothing anywhere records that the thing was ever proposed.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// How far back the read looks. A week covers two full 72-hour approval
/// windows, so an operator returning from a few days away sees everything that
/// died while they were gone, and nothing so old that it is history rather
/// than a loss they could have prevented.
pub const LAPSE_WINDOW_DAYS: i64 = 7;

/// How many lapsed asks come back. The count is reported separately and is not
/// capped, so a truncated list still says how much it is hiding.
const MAX_LAPSED: i64 = 25;

/// One ask that is past answering.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LapsedApproval {
    pub action_kind: String,
    pub context: String,
    pub subject_kind: String,
    /// `approval_expired`, `insufficient_evidence` or `awaiting_sweep`. Three
    /// deaths, three different things to do about them — merging them into one
    /// "expired" bucket would make the operator's own unanswered queue look the
    /// same as the machine withdrawing a question it should not have asked.
    pub cause: String,
    /// When the sweep cancelled it. `NULL` for `awaiting_sweep`, which has not
    /// been cancelled yet — absent, not zero and not "now".
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// When the window closed. Always present for the two cancelled causes
    /// unless the row predates the column being set.
    #[serde(with = "time::serde::rfc3339::option")]
    pub approval_expires_at: Option<OffsetDateTime>,
    /// The decision's own sentence — why the machine proposed this in the first
    /// place. Without it the entry is a kind and a timestamp, and an operator
    /// cannot tell whether what lapsed was worth chasing.
    pub reason: String,
}

/// The queue's losses, and the pressure on it right now.
#[derive(Debug, Serialize)]
pub struct LapsedApprovals {
    pub window_days: i64,
    /// Newest first, capped at [`MAX_LAPSED`].
    pub items: Vec<LapsedApproval>,
    /// Every lapse in the window, including any the list omitted.
    pub total: i64,
    /// Pending asks whose window closes within a day. The forward-looking half:
    /// the list above is what was already lost, this is what is about to be.
    pub expiring_within_24h: i64,
}

/// One row as the query returns it: a lapsed ask plus the window count, which
/// rides along so the total does not cost a second round trip.
#[derive(Debug, sqlx::FromRow)]
struct LapsedRow {
    action_kind: String,
    context: String,
    subject_kind: String,
    cause: String,
    finished_at: Option<OffsetDateTime>,
    approval_expires_at: Option<OffsetDateTime>,
    reason: String,
    total_count: i64,
}

/// Reads what lapsed in the last [`LAPSE_WINDOW_DAYS`] days, and what is about
/// to.
///
/// # Errors
///
/// Propagates the database error.
pub async fn lapsed_approvals(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<LapsedApprovals, sqlx::Error> {
    let rows = sqlx::query_as::<_, LapsedRow>(
        r#"
            SELECT
                action.action_kind,
                action.context,
                action.subject_kind,
                CASE
                    WHEN action.status = 'awaiting_approval' THEN 'awaiting_sweep'
                    ELSE action.last_error_kind
                END AS cause,
                action.finished_at,
                action.approval_expires_at,
                decision.reason,
                count(*) OVER ()::bigint AS total_count
            FROM viryaos_autopilot_actions AS action
            JOIN viryaos_autopilot_decisions AS decision
              ON decision.workspace_id = action.workspace_id
             AND decision.id = action.decision_id
            WHERE action.workspace_id = $1
              AND (
                    -- Reaped by a sweep — the claim path's per-cycle pass or
                    -- the retention worker's hourly global one.
                    (
                        action.status = 'cancelled'
                        AND action.last_error_kind IN ('approval_expired', 'insufficient_evidence')
                        AND action.finished_at IS NOT NULL
                        AND action.finished_at > $2 - make_interval(days => $3::int)
                    )
                    -- Past its deadline and not yet reaped. Both sweeps run
                    -- independently of claim volume, so a row here means a
                    -- workspace neither reached — parked, disabled, or a
                    -- stalled worker; `approval.sweep_lagging` alarms on the
                    -- same population past two sweep intervals.
                 OR (
                        action.status = 'awaiting_approval'
                        AND action.approval_expires_at IS NOT NULL
                        AND action.approval_expires_at <= $2
                        AND action.approval_expires_at > $2 - make_interval(days => $3::int)
                    )
              )
            ORDER BY COALESCE(action.finished_at, action.approval_expires_at) DESC, action.id DESC
            LIMIT $4
            "#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(i32::try_from(LAPSE_WINDOW_DAYS).unwrap_or(7))
    .bind(MAX_LAPSED)
    .fetch_all(pool)
    .await?;

    let total = rows.first().map_or(0, |row| row.total_count);
    let items = rows
        .into_iter()
        .map(|row| LapsedApproval {
            action_kind: row.action_kind,
            context: row.context,
            subject_kind: row.subject_kind,
            cause: row.cause,
            finished_at: row.finished_at,
            approval_expires_at: row.approval_expires_at,
            reason: row.reason,
        })
        .collect();

    let expiring_within_24h = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM viryaos_autopilot_actions AS action
        WHERE action.workspace_id = $1
          AND action.status = 'awaiting_approval'
          AND action.approval_expires_at IS NOT NULL
          AND action.approval_expires_at > $2
          AND action.approval_expires_at <= $2 + INTERVAL '24 hours'
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_one(pool)
    .await?;

    Ok(LapsedApprovals {
        window_days: LAPSE_WINDOW_DAYS,
        items,
        total,
        expiring_within_24h,
    })
}
