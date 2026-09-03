//! What the operator and the measurements have said about each worker.
//!
//! Two signals, read from the same rows and both about recent behaviour:
//! `Standing` is what measurement outcomes say a template is worth, and the
//! preference posterior is what the operator's approvals say they want.
//!
//! Split out of the snapshot loader because it is a self-contained read and
//! the loader had grown past the size ratchet. Both queries used to scan
//! without a bound, and the preference posterior ran a second scan over the
//! same join because the first query had not selected the column it needed.

use super::*;

/// Standings per template, and the tenant's operating preference.
pub(super) struct WorkerSignals {
    pub(super) standings: std::collections::HashMap<String, Standing>,
    pub(super) tenant_preference: TenantPreferencePosterior,
}

pub(super) async fn load_worker_signals(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<WorkerSignals, RepositoryError> {
    // Load agent standings from past measurement outcomes. The brain learns
    // which worker templates produce fan growth and which don't, and adjusts
    // dispatch cadence accordingly. We load raw outcomes ordered by
    // observed_at DESC and compute the OutcomeRecord (with
    // consecutive_worsened) in Rust — simpler and more testable than a
    // window-function SQL approach.
    //
    // Bounded to the same window as operator feedback. Standing is about how
    // a worker has been performing lately; scanning every outcome a workspace
    // has ever recorded, every cycle, buys nothing a year-old result can
    // still say.
    let standing_rows: Vec<(String, String, OffsetDateTime)> = sqlx::query_as(
        r#"
        SELECT action.payload->>'template_id' AS template_id,
               outcome.effect_assessment,
               outcome.observed_at
        FROM viryaos_autopilot_outcomes outcome
        JOIN viryaos_autopilot_actions action ON action.id = outcome.action_id
        WHERE action.workspace_id = $1
          AND action.action_kind = 'agent.run.request'
          AND outcome.effect_assessment IS NOT NULL
          AND outcome.observed_at > now() - ($2 || ' days')::interval
        ORDER BY action.payload->>'template_id', outcome.observed_at DESC
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(OPERATOR_FEEDBACK_WINDOW_DAYS.to_string())
    .bind(OPERATOR_FEEDBACK_MAX_ROWS)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // Build OutcomeRecord per template by iterating the ordered rows.
    let policy = agent_standing_policy();

    // Load operator feedback (approve/cancel verdicts) from operator_actions.
    // This is the execution-quality signal: "would a human operator accept
    // this draft?" — separate from the causal fan-growth signal above. We
    // join operator_actions with autopilot_actions to get the template_id
    // and time-to-decision (operator_action.created_at - action.created_at).
    // Fast = within 1 hour (configurable via GrowthIntelligencePolicy).
    // Operator feedback, read once for the two things derived from it: how
    // fast the operator decided (execution quality) and how long ago they
    // decided (the preference posterior's decay).
    //
    // These were two queries over the same join, one of them commented
    // "query it fresh since the existing rows only carry hours_to_decision,
    // not age" — so the loader scanned every operator action this workspace
    // has ever taken, twice, every cycle. Selecting both columns once costs
    // nothing and removes a whole scan.
    //
    // Bounded by `OPERATOR_FEEDBACK_WINDOW_DAYS`. The preference posterior
    // decays on a 90-day half-life, so a decision from a year ago carries
    // about 6% of a fresh one's weight, and standing is a statement about
    // recent behaviour. Reading history without end made the cycle's cost
    // grow with the workspace's age for signal that had already decayed to
    // nothing.
    let operator_feedback_rows: Vec<(String, String, f64, f64)> = sqlx::query_as(
        r#"
        SELECT COALESCE(action.payload->>'template_id', 'unknown') AS template_id,
               oa.action AS operator_action,
               EXTRACT(EPOCH FROM (oa.created_at - action.created_at)) / 3600.0 AS hours_to_decision,
               EXTRACT(EPOCH FROM (now() - oa.created_at)) / 86400.0 AS age_days
        FROM operator_actions oa
        JOIN viryaos_autopilot_actions action ON action.id = oa.target_id
        -- Both sides carry the workspace. Constraining only the joined
        -- action left `operator_actions` with no indexable predicate, and
        -- every index on it leads with workspace_id — so the planner read
        -- the whole table. The join already pins the workspace, so this
        -- narrows nothing and changes a sequential scan into an index scan.
        WHERE oa.workspace_id = $1
          AND action.workspace_id = $1
          AND action.action_kind = 'agent.run.request'
          AND oa.target_type = 'autopilot_action'
          AND oa.action IN ('approve_autopilot_action', 'cancel_autopilot_action')
          AND oa.created_at > now() - ($2 || ' days')::interval
        ORDER BY action.payload->>'template_id', oa.created_at DESC
        LIMIT $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(OPERATOR_FEEDBACK_WINDOW_DAYS.to_string())
    .bind(OPERATOR_FEEDBACK_MAX_ROWS)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let operator_fast_threshold_hours = 1.0_f64;

    let standings: std::collections::HashMap<String, Standing> = {
        let mut records: std::collections::HashMap<String, OutcomeRecord> =
            std::collections::HashMap::new();
        for (template_id, assessment, _observed_at) in &standing_rows {
            let record = records.entry(template_id.clone()).or_default();
            record.improved += u32::from(assessment == "improved");
            record.neutral += u32::from(assessment == "neutral");
            record.worsened += u32::from(assessment == "worsened");
            // consecutive_worsened: count from the most recent (first in the
            // DESC-ordered rows) until we hit a non-worsened outcome.
            if assessment == "worsened" {
                // Only increment if all prior rows (more recent) were also
                // worsened. We check by seeing if improved+neutral is still 0.
                if record.improved == 0 && record.neutral == 0 {
                    record.consecutive_worsened += 1;
                }
            }
            // else: the streak is broken — but we've already counted the
            // improved/neutral, so the condition above naturally stops
            // incrementing consecutive_worsened for any older worsened rows.
        }
        // Fold operator feedback into the records. Operator feedback does NOT
        // update consecutive_worsened — only measured fan-growth outcomes can
        // trigger retirement. Operator cancellations affect the weight (and
        // thus cooldown) but cannot retire a worker on their own.
        for (template_id, operator_action, hours_to_decision, _age_days) in &operator_feedback_rows
        {
            let record = records.entry(template_id.clone()).or_default();
            let approved = operator_action == "approve_autopilot_action";
            let fast = *hours_to_decision <= operator_fast_threshold_hours;
            *record = (*record).observe_operator(approved, fast);
        }
        records
            .into_iter()
            .map(|(template_id, record)| (template_id, assess_standing(record, policy)))
            .collect()
    };

    // ── Tenant operating preference ──
    // Build a TenantPreferencePosterior from the same raw operator actions,
    // but interpreted as "does this tenant prefer this template?" rather than
    // "is this execution acceptable?". Uses exponentially decayed evidence
    // (90-day half-life default) so preferences can shift over time.
    //
    // This is a SEPARATE belief from Standing. Both consume the same raw
    // operator events but answer different questions:
    //   Standing        → "Is this worker performing well?" → cooldown/tier
    //   TenantPreference → "Does this tenant prefer this template?" → cadence
    //
    // The preference posterior MUST NOT modify DecisionValue or any economic
    // value. It only influences cadence timing. Presentation metadata is
    // derived brain-side but currently NOT persisted (TODO: wire to operator
    // read path when the UI supports it).
    let tenant_preference: TenantPreferencePosterior = {
        let mut posterior = TenantPreferencePosterior::new();
        // The same rows the standing calculation used, decayed by age
        // instead of classified fast/slow.
        // The half-life matches TenantPreferencePolicy::default().half_life_days
        // (90 days). The infra layer doesn't receive the GrowthIntelligencePolicy
        // (it's loaded in the application layer), so we use the default here.
        // Future: wire the policy through if per-workspace customization is needed.
        let half_life = 90.0_f64;
        for (template_id, operator_action, _hours, age_days) in &operator_feedback_rows {
            let approved = operator_action == "approve_autopilot_action";
            posterior.observe(template_id, approved, *age_days, half_life);
        }
        posterior
    };

    Ok(WorkerSignals {
        standings,
        tenant_preference,
    })
}
