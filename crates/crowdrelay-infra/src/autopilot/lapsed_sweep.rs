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
    /// Letters withdrawn because the opportunity they were written for
    /// retired — the contact stopped being one this subject is news to, the
    /// show moved or was cancelled, or its pitch window closed.
    pub opportunities_retired: u64,
    /// Community drafts withdrawn because they are not written in their
    /// community's language.
    pub community_language_mismatches: u64,
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
            last_error_kind = 'approval_expired',
            -- Re-key the dead row so the same proposal may be raised again:
            -- the key's job is to dedupe a *live* ask, and an expired ask
            -- holding the key forever meant a proposal nobody answered
            -- could never re-raise — measured in production, fifty-one of
            -- them died silently in fourteen days. The suffix keeps the
            -- dead row's key unique (its own id), the audit trail intact,
            -- and the next evaluation free to mint the same ask afresh.
            idempotency_key = idempotency_key || ':lapsed:' || action.id::text
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

    // A letter whose opportunity retired cannot be sent.
    //
    // `lock_outreach_for_execution` refuses a retired or expired opportunity,
    // so approving one of these ends in a bare conflict and nothing leaves.
    // Safe, but the operator is the throughput limit and every such row is a
    // click that can only fail. On 2026-09-27, after #322 narrowed show
    // letters to the show's country, fifteen of the thirty-seven queued
    // letters for the Gorzów show were addressed to contacts in Germany,
    // Austria, Czechia, Slovakia or behind a free-mail address, and their
    // opportunities had already retired underneath them. The supply refresh
    // retires opportunities every cycle; this is the same fact applied to the
    // asks written against them. Re-keyed like an expired ask, so an
    // opportunity that comes back live — a show re-published — may be
    // written for again.
    let opportunities_retired = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT action.workspace_id, action.id
            FROM autopilot_actions AS action
            WHERE action.status = 'awaiting_approval'
              AND action.action_kind = 'outreach.request'
              AND action.payload ? 'opportunity_id'
              AND ($1::uuid IS NULL OR action.workspace_id = $1)
              AND NOT EXISTS (
                  SELECT 1 FROM outreach_opportunities AS opportunity
                  WHERE opportunity.workspace_id = action.workspace_id
                    AND opportunity.id::text = action.payload->>'opportunity_id'
                    AND opportunity.active
                    AND opportunity.expires_at > $2
              )
            ORDER BY action.approval_expires_at NULLS LAST, action.id
            FOR UPDATE OF action SKIP LOCKED
            LIMIT $3
        )
        UPDATE autopilot_actions AS action
        SET status = 'cancelled',
            finished_at = $2,
            last_error_kind = 'opportunity_retired',
            idempotency_key = idempotency_key || ':retired:' || action.id::text
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

    // A community draft in the wrong language cannot be approved into a post.
    //
    // The ingest gate (`community_language_mismatch` in agent_outcomes.rs)
    // reads every new draft since #323, but it runs when an outcome arrives,
    // and drafts queued before it were never read. On 2026-09-27 two were
    // still waiting: "Wariacie wpadasz na gigusa?" for r/deathmetal and
    // r/metalcore, both English-language, the day after a moderator removed
    // the band's Polish caption from r/melodicdeathmetal. The same rule,
    // applied to the asks already in the queue — language detection is Rust,
    // so candidates are read, judged here and withdrawn by id.
    let community_candidates =
        sqlx::query_as::<_, (Uuid, Uuid, Option<String>, Option<String>, Option<String>)>(
            r#"
        SELECT action.workspace_id, action.id,
               action.payload->>'title', action.payload->>'body', target.language
        FROM autopilot_actions AS action
        LEFT JOIN agent_outreach_targets AS target
          ON target.workspace_id = action.workspace_id
         AND target.id::text = action.payload->>'target_id'
        WHERE action.status = 'awaiting_approval'
          AND action.action_kind = 'community.engage.request'
          AND ($1::uuid IS NULL OR action.workspace_id = $1)
        ORDER BY action.created_at, action.id
        FOR UPDATE OF action SKIP LOCKED
        LIMIT $2
        "#,
        )
        .bind(workspace_uuid)
        .bind(limit)
        .fetch_all(&mut **transaction)
        .await?;
    let mismatched: Vec<Uuid> = community_candidates
        .into_iter()
        .filter(|(_, _, title, body, language)| {
            let text = [title.as_deref(), body.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n");
            crowdrelay_domain::community_language::community_language_mismatch(
                &text,
                language.as_deref(),
            )
            .is_some()
        })
        .map(|(_, id, _, _, _)| id)
        .collect();
    let community_language_mismatches = if mismatched.is_empty() {
        0
    } else {
        sqlx::query(
            r#"
            UPDATE autopilot_actions
            SET status = 'cancelled',
                finished_at = $2,
                last_error_kind = $3,
                idempotency_key = idempotency_key || ':language:' || id::text
            WHERE id = ANY($1::uuid[])
              AND status = 'awaiting_approval'
              AND ($4::uuid IS NULL OR workspace_id = $4)
            "#,
        )
        .bind(&mismatched)
        .bind(now)
        .bind(crowdrelay_domain::community_language::COMMUNITY_LANGUAGE_MISMATCH)
        .bind(workspace_uuid)
        .execute(&mut **transaction)
        .await?
        .rows_affected()
    };

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
        opportunities_retired,
        community_language_mismatches,
        suggestions_expired,
        arcs_retired,
    })
}

/// Refuses a community draft that is not in its community's language, at the
/// last point before it is emitted. An approval given before the ingest gate
/// existed — or a batch approval given on one sample — must not publish a
/// draft in the wrong language; [`sweep_lapsed_approval_asks`] withdraws such
/// drafts from the queue, and this is the guard for one already approved.
pub(in crate::autopilot) async fn refuse_draft_in_wrong_language(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    target_id: &str,
    draft: &str,
) -> Result<(), RepositoryError> {
    let community_language = sqlx::query_scalar::<_, Option<String>>(
        "SELECT language FROM agent_outreach_targets WHERE workspace_id = $1 AND id::text = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .flatten();
    if crowdrelay_domain::community_language::community_language_mismatch(
        draft,
        community_language.as_deref(),
    )
    .is_some()
    {
        return Err(RepositoryError::ConflictBecause(
            crowdrelay_domain::community_language::COMMUNITY_LANGUAGE_MISMATCH,
        ));
    }
    Ok(())
}
