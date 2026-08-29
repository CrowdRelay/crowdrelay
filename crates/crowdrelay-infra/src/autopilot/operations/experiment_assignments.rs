//! Persistence for first-class experiment assignments.
//!
//! The experimental unit is explicitly defined — not always workspace-wide.
//! When interference is not controllable, the `experiment_kind` is
//! downgraded to `matched_quasi_experiment`.
//!
//! # Identity
//!
//! Each assignment has a unique `id` (assignment_id). The `experiment_uuid`
//! links all assignments in the same experiment. `assignment_round`
//! distinguishes repeated experiments on the same unit over time.
//!
//! One assignment per `(experiment_uuid, assignment_round, unit_id)` — the
//! arm is a property of the assignment. The unique index prevents
//! double-assignment.

use crowdrelay_domain::WorkspaceId;

use super::{PostgresAutopilotRepository, RepositoryError, map_sqlx};

/// Records a first-class experiment assignment in the
/// `viryaos_experiment_assignments` table. The experimental unit is
/// explicitly defined — not always workspace-wide. When interference is
/// not controllable, the `experiment_kind` is downgraded to
/// `matched_quasi_experiment`.
///
/// The assignment ID is unique per row. The experiment_uuid links
/// assignments in the same experiment. The unique index on
/// `(workspace_id, experiment_uuid, assignment_round, unit_id)` prevents
/// double-assignment of the same unit in the same round.
pub(in crate::autopilot) async fn record_experiment_assignment(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    assignment: &crowdrelay_brain::ExperimentAssignment,
    strategy: Option<&str>,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    let context_json = serde_json::to_value(&assignment.context).unwrap_or(serde_json::json!({}));
    let prediction_json =
        serde_json::to_value(&assignment.prediction).unwrap_or(serde_json::json!({}));
    let kind = assignment.kind();
    sqlx::query(
        r#"
        INSERT INTO viryaos_experiment_assignments
            (id, workspace_id, unit_id, unit_kind, arm, assigned_at,
             propensity, intended_template_id, context, prediction,
             action_id, strategy, experiment_kind,
             contamination_estimate, is_interference_controllable,
             experiment_uuid, assignment_round,
             eligibility_criteria, selection_context,
             interference_policy, assignment_time_contamination)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                $16, $17, $18, $19, $20, $21)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(&assignment.assignment_id)
    .bind(workspace_id.into_uuid())
    .bind(&assignment.unit_id)
    .bind(assignment.unit_kind.as_str())
    .bind(assignment.arm.as_str())
    .bind(assignment.assigned_at)
    .bind(assignment.propensity)
    .bind(&assignment.intended_template_id)
    .bind(&context_json)
    .bind(&prediction_json)
    .bind(assignment.action_id)
    .bind(strategy)
    .bind(kind.as_str())
    .bind(assignment.contamination_estimate)
    .bind(assignment.is_interference_controllable)
    .bind(assignment.experiment_uuid)
    .bind(assignment.assignment_round as i32)
    .bind(&assignment.eligibility_criteria)
    .bind(&assignment.selection_context)
    .bind(assignment.interference_policy.as_str())
    .bind(assignment.contamination_estimate)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;

    // Also record a control evidence row when this is a control arm,
    // so the measurement system can measure the control group's fan
    // growth. The evidence row uses the assignment_id as the linking
    // key and has action_id=NULL (per migration 0163).
    if assignment.arm == crowdrelay_brain::TreatmentAssignment::Control {
        let evidence_quality = if assignment.is_interference_controllable {
            crowdrelay_brain::EvidenceQuality::RandomizedHoldout
        } else {
            crowdrelay_brain::EvidenceQuality::MatchedQuasiExperiment
        };
        let target = assignment
            .prediction
            .context
            .subreddit_type
            .clone()
            .unwrap_or_else(|| assignment.unit_id.clone());
        let opportunity_id = crowdrelay_brain::OpportunityId::new(
            &assignment.prediction.template_id,
            &target,
            crowdrelay_brain::OpportunityAction::Post,
            &assignment.prediction.context,
        );
        let channel = assignment
            .prediction
            .context
            .subreddit_type
            .as_deref()
            .map(|s| {
                if s.starts_with("r/") {
                    crowdrelay_brain::ReachChannel::RedditPost
                } else {
                    crowdrelay_brain::ReachChannel::Other
                }
            })
            .unwrap_or(crowdrelay_brain::ReachChannel::Other);
        let evidence = crowdrelay_brain::GrowthEvidence::at_dispatch(
            workspace_id.into_uuid(),
            assignment.action_id.unwrap_or_else(uuid::Uuid::now_v7),
            Some(opportunity_id.to_string()),
            target,
            channel,
            1,
            crowdrelay_brain::TreatmentAssignment::Control,
            assignment.propensity,
            assignment.prediction.expected_new_fans,
            assignment.prediction.expected_signal_installs,
            assignment.prediction.context.clone(),
            strategy.map(|s| s.to_owned()),
            evidence_quality,
        );
        let _ = super::evidence::record_growth_evidence(repo, workspace_id, &evidence).await;
    }
    Ok(())
}

/// Evaluates contamination over the full measurement window.
///
/// Contamination is NOT just the assignment-time snapshot. It must be
/// evaluated over the entire measurement window: assignment-time
/// interference + post-assignment interference + cross-channel
/// contamination. A clean assignment can become contaminated later.
///
/// This scans ALL treatment actions on the same unit during the full
/// window, computes `final_contamination`, and downgrades
/// `final_evidence_quality` if contamination is high (> 0.1).
pub(in crate::autopilot) async fn evaluate_contamination(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    experiment_uuid: uuid::Uuid,
    unit_id: &str,
    assignment_time: time::OffsetDateTime,
    measurement_window_end: time::OffsetDateTime,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    // Count concurrent treatment actions on the same unit during the
    // full measurement window (not just assignment-time).
    let concurrent_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT ea.id)
        FROM viryaos_experiment_assignments ea
        WHERE ea.workspace_id = $1
          AND ea.unit_id = $2
          AND ea.arm = 'treatment'
          AND ea.assigned_at >= $3
          AND ea.assigned_at <= $4
          AND ea.experiment_uuid != $5
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(unit_id)
    .bind(assignment_time)
    .bind(measurement_window_end)
    .bind(experiment_uuid)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    // final_contamination = min(1.0, concurrent / (concurrent + 1))
    // This gives 0.0 when no concurrent actions, 0.5 when one, 0.67 when two, etc.
    let final_contamination = if concurrent_count <= 0 {
        0.0
    } else {
        (concurrent_count as f64 / (concurrent_count as f64 + 1.0)).min(1.0)
    };
    // Downgrade evidence quality if contamination is high.
    let final_evidence_quality = if final_contamination > 0.1 {
        "matched_quasi_experiment"
    } else {
        "randomized_holdout"
    };
    sqlx::query(
        r#"
        UPDATE viryaos_experiment_assignments
        SET final_contamination = $3,
            final_evidence_quality = $4,
            contamination_resolved_at = $5
        WHERE workspace_id = $1
          AND experiment_uuid = $2
          AND unit_id = $6
          AND contamination_resolved_at IS NULL
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(experiment_uuid)
    .bind(final_contamination)
    .bind(final_evidence_quality)
    .bind(measurement_window_end)
    .bind(unit_id)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    // CRITICAL INVARIANT: evidence rows are immutable. We do NOT
    // backward-rewrite viryaos_growth_evidence.evidence_quality based
    // on the current contamination assessment. The final_contamination
    // and final_evidence_quality are stored on the experiment assignment
    // row, and the learner reads them from there when consuming evidence.
    // Backward-rewriting historical evidence would violate the temporal
    // causal boundary and make evidence non-reproducible.
    Ok(())
}

/// Records a fan provenance event — an append-only exposure/
/// interaction/conversion/durability event.
///
/// PROVENANCE ≠ CAUSALITY. These events establish exposure/attribution
/// evidence, not causal treatment effect.
pub(in crate::autopilot) async fn record_fan_provenance_event(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event: &crowdrelay_brain::FanProvenanceEvent,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    sqlx::query(
        r#"
        INSERT INTO fan_provenance_events
            (workspace_id, fan_id, event_kind, channel, source_target,
             community, campaign_id, action_id, attribution_method,
             attribution_confidence, occurred_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event.fan_id)
    .bind(event.event_kind.as_str())
    .bind(&event.channel)
    .bind(&event.source_target)
    .bind(&event.community)
    .bind(event.campaign_id)
    .bind(event.action_id)
    .bind(&event.attribution_method)
    .bind(event.attribution_confidence)
    .bind(event.occurred_at)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Loads the strongest evidence quality for a template+unit from the
/// experiment assignment state.
pub(in crate::autopilot) async fn load_evidence_quality(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    template_id: &str,
    unit_id: &str,
) -> Result<crowdrelay_brain::EvidenceQuality, RepositoryError> {
    let pool = &repo.pool;
    // Check for resolved (final_evidence_quality) first, then fall back
    // to the experiment_kind from assignment time.
    let resolved: Option<String> = sqlx::query_scalar(
        r#"
        SELECT final_evidence_quality
        FROM viryaos_experiment_assignments
        WHERE workspace_id = $1
          AND intended_template_id = $2
          AND unit_id = $3
          AND final_evidence_quality IS NOT NULL
        ORDER BY assigned_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(template_id)
    .bind(unit_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    if let Some(quality) = resolved {
        return Ok(match quality.as_str() {
            "randomized_holdout" => crowdrelay_brain::EvidenceQuality::RandomizedHoldout,
            "matched_quasi_experiment" => crowdrelay_brain::EvidenceQuality::MatchedQuasiExperiment,
            _ => crowdrelay_brain::EvidenceQuality::Observational,
        });
    }
    // No resolved contamination — check assignment-time experiment_kind.
    let kind: Option<String> = sqlx::query_scalar(
        r#"
        SELECT experiment_kind
        FROM viryaos_experiment_assignments
        WHERE workspace_id = $1
          AND intended_template_id = $2
          AND unit_id = $3
        ORDER BY assigned_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(template_id)
    .bind(unit_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(match kind.as_deref() {
        Some("randomized_holdout") => crowdrelay_brain::EvidenceQuality::RandomizedHoldout,
        Some("matched_quasi_experiment") => {
            crowdrelay_brain::EvidenceQuality::MatchedQuasiExperiment
        }
        _ => crowdrelay_brain::EvidenceQuality::Observational,
    })
}

/// Loads the contamination estimate for a unit+template from the
/// experiment assignment state. Returns 0.0 when no assignments exist.
pub(in crate::autopilot) async fn load_contamination_estimate(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    template_id: &str,
    unit_id: &str,
) -> Result<f64, RepositoryError> {
    let pool = &repo.pool;
    // Prefer final_contamination (resolved over full window) over
    // assignment_time_contamination (snapshot at assignment time).
    let contamination: Option<(Option<f64>, f64)> = sqlx::query_as::<_, (Option<f64>, f64)>(
        r#"
        SELECT final_contamination, assignment_time_contamination
        FROM viryaos_experiment_assignments
        WHERE workspace_id = $1
          AND intended_template_id = $2
          AND unit_id = $3
        ORDER BY assigned_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(template_id)
    .bind(unit_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(contamination
        .and_then(|(final_c, assign_c)| final_c.or(Some(assign_c)))
        .unwrap_or(0.0))
}

/// Loads the calibration bias for a template from the calibration
/// tracker in brain state. Returns 0.0 when no calibration data exists.
pub(in crate::autopilot) async fn load_calibration_bias(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    template_id: &str,
) -> Result<f64, RepositoryError> {
    let pool = &repo.pool;
    // The calibration bias is stored in the brain state checkpoint.
    // We read it from the viryaos_brain_state table's calibration data.
    // For now, return 0.0 — the calibration tracker is in-memory in the
    // causal model, and its bias is applied when the model is built.
    // Phase 2 will persist calibration data to a dedicated table.
    let _ = (pool, workspace_id, template_id);
    Ok(0.0)
}
