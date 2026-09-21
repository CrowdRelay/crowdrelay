//! Sweeping dead approval asks.
//!
//! The claim path runs this sweep for its workspace on every cycle, which
//! covers tenants whose worker is alive and whose autopilot is enabled. It
//! does not run for a parked tenant, a disabled one, or a workspace whose
//! claim loop is starved — and `load_needs_you` hides expired rows, so the
//! dead would pile up invisibly. The retention worker runs the same sweep
//! globally once an hour. Both call this function so the semantics of "an
//! ask died" exist in exactly one place.

use super::*;

/// What one sweep pass resolved. The claim path ignores the counts — the
/// work is the point; the retention worker reports them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LapsedSweepStats {
    /// Asks that outlived `approval_expires_at` unanswered.
    pub approvals_expired: u64,
    /// Asks withdrawn because the decision behind them carried zero
    /// confidence — a connector failure wearing the shape of a proposal,
    /// not a question a person could answer.
    pub insufficient_evidence: u64,
    /// Content suggestions flipped to `expired` because their ask died.
    pub suggestions_expired: u64,
    /// Proposed arcs retired for the same reason.
    pub arcs_retired: u64,
}

/// Applies every death an `awaiting_approval` row can die, plus the
/// cascades that keep the subjects honest.
///
/// `workspace_id` scopes the pass (`Some` in the claim path); `None` sweeps
/// every workspace — the retention worker's mode, which is what makes the
/// sweep reach parked tenants and disabled autopilots. `limit` bounds the
/// per-pass work (`None` = unbounded, as the claim path has always been;
/// `Some` for retention's batch size). Re-runs are no-ops: every transition
/// is guarded on the live status.
///
/// The subject cascades match on *any* matching dead ask, not only the ones
/// this pass reaped — so a workspace that accumulated `awaiting_sweep` rows
/// before the global sweep existed still resolves their suggestions and
/// arcs on the next pass. The suggestion's own `raised` guard keeps the
/// outcome insert single-fire.
///
/// # Errors
/// Returns the underlying `sqlx::Error`; each caller maps it into its own
/// error type.
pub async fn sweep_lapsed_approval_asks(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Option<WorkspaceId>,
    now: OffsetDateTime,
    limit: Option<i64>,
) -> Result<LapsedSweepStats, sqlx::Error> {
    let workspace_uuid = workspace_id.map(WorkspaceId::into_uuid);

    let approvals_expired = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT workspace_id, id
            FROM autopilot_actions
            WHERE status = 'awaiting_approval'
              AND approval_expires_at IS NOT NULL
              AND approval_expires_at <= $2
              AND ($1::uuid IS NULL OR workspace_id = $1)
            ORDER BY approval_expires_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT $3
        )
        UPDATE autopilot_actions AS action
        SET status = 'cancelled',
            finished_at = $2,
            last_error_kind = 'approval_expired'
        FROM candidates
        WHERE action.workspace_id = candidates.workspace_id
          AND action.id = candidates.id
          AND action.status = 'awaiting_approval'
        "#,
    )
    .bind(workspace_uuid)
    .bind(now)
    .bind(limit)
    .execute(&mut **transaction)
    .await?
    .rows_affected();

    // An approval nobody can answer is not a decision.
    //
    // `evaluate_outcome_quality` refuses to create these now: a
    // `require_approval` outcome with zero confidence produces no decision
    // at all. That guard is prospective, and it left the ones already in the
    // queue where they were. Production carried one — "Zatwierdź cel
    // outreach: Unnamed target", 0% confidence, whose own stated reason is
    // that all three Reddit searches returned credential errors and no
    // subreddit data was retrieved.
    //
    // An operator cannot approve that and cannot reject it as wrong,
    // because it is not a proposal about the world; it is a connector
    // failure wearing the shape of one. Left alone it would sit in the
    // queue until `approval_expires_at` reaped it days later, teaching the
    // operator that the queue contains things to ignore — which is the
    // habit that makes an exception queue worthless.
    //
    // Same rule as the ingress guard, applied to state rather than to
    // arrivals, so a guard added after the rows exist still finishes the
    // job. No attempt row: this action was never attempted, and the expiry
    // sweep above does not write one either.
    let insufficient_evidence = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT action.workspace_id, action.id
            FROM autopilot_actions AS action
            JOIN autopilot_decisions AS decision
              ON decision.workspace_id = action.workspace_id
             AND decision.id = action.decision_id
            WHERE action.status = 'awaiting_approval'
              AND decision.confidence_basis_points = 0
              AND ($1::uuid IS NULL OR action.workspace_id = $1)
            ORDER BY action.approval_expires_at NULLS LAST, action.id
            FOR UPDATE OF action SKIP LOCKED
            LIMIT $3
        )
        UPDATE autopilot_actions AS action
        SET status = 'cancelled',
            finished_at = $2,
            last_error_kind = 'insufficient_evidence'
        FROM candidates
        WHERE action.workspace_id = candidates.workspace_id
          AND action.id = candidates.id
          AND action.status = 'awaiting_approval'
        "#,
    )
    .bind(workspace_uuid)
    .bind(now)
    .bind(limit)
    .execute(&mut **transaction)
    .await?
    .rows_affected();

    // A suggestion whose ask died in the queue — window lapsed or evidence
    // too thin to ask — is itself dead. Without this pair it stays `raised`
    // forever: invisible to the evaluator (which skips lapsed rows),
    // uncountable as a lesson, and holding a slot in the three-deep open
    // queue until nothing new can be suggested.
    //
    // The EXISTS matches any matching dead ask rather than only the rows
    // this pass reaped: suggestions orphaned by the pre-retention claim-path
    // sweep (a parked tenant's asks died hours after the last claim) heal on
    // the next pass instead of lingering forever.
    let suggestions_expired = sqlx::query(
        r#"
        WITH resolved AS (
            UPDATE content_suggestions AS suggestion
            SET status = 'expired', updated_at = $2
            WHERE suggestion.status = 'raised'
              AND ($1::uuid IS NULL OR suggestion.workspace_id = $1)
              AND EXISTS (
                  SELECT 1 FROM autopilot_actions AS action
                  WHERE action.workspace_id = suggestion.workspace_id
                    AND action.subject_kind = 'content_suggestion'
                    AND action.subject_id = suggestion.id
                    AND action.status = 'cancelled'
                    AND action.last_error_kind
                        IN ('approval_expired', 'insufficient_evidence')
              )
            RETURNING suggestion.id, suggestion.workspace_id
        )
        INSERT INTO suggestion_outcomes (
            workspace_id, suggestion_id, outcome, decided_by, reason
        )
        SELECT resolved.workspace_id, resolved.id, 'expired', 'system',
               'the approval window closed unanswered'
        FROM resolved
        "#,
    )
    .bind(workspace_uuid)
    .bind(now)
    .execute(&mut **transaction)
    .await?
    .rows_affected();

    // The same death, one level up: a proposed arc whose ask lapsed retires
    // — the open-arc check counts `proposed` as live, so a zombie here would
    // block every future season silently. The anchor's cooldown keeps the
    // next proposal honest. Same any-dead-ask generalization as the
    // suggestion cascade above.
    let arcs_retired = sqlx::query(
        r#"
        UPDATE arcs AS arc
        SET status = 'retired', updated_at = $2
        WHERE arc.status = 'proposed'
          AND ($1::uuid IS NULL OR arc.workspace_id = $1)
          AND EXISTS (
              SELECT 1 FROM autopilot_actions AS action
              WHERE action.workspace_id = arc.workspace_id
                AND action.subject_kind = 'content_arc'
                AND action.subject_id = arc.id
                AND action.status = 'cancelled'
                AND action.last_error_kind
                    IN ('approval_expired', 'insufficient_evidence')
          )
        "#,
    )
    .bind(workspace_uuid)
    .bind(now)
    .execute(&mut **transaction)
    .await?
    .rows_affected();

    Ok(LapsedSweepStats {
        approvals_expired,
        insufficient_evidence,
        suggestions_expired,
        arcs_retired,
    })
}
