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

/// Get-or-creates a persisted experiment design.
///
/// P0-1: The experiment identity must survive evaluator retries. The same
/// `(workspace, intervention_key, logical_cycle_key)` always converges on
/// the same `experiment_uuid`. On first call, a new design is inserted with
/// a fresh UUID. On retry or concurrent call, the existing design is
/// returned unchanged.
///
/// P1-a (immutability): On retry, the PERSISTED design is returned — not a
/// reconstruction from the current caller's inputs. The design is immutable
/// after creation: `eligible_units`, `estimand`, `holdout_probability`,
/// `interference_policy`, and `unit_kind` are set once at first creation
/// and never mutated. `check_power()` runs ONLY on first creation; on retry
/// the persisted status and counts are returned as-is.
#[allow(clippy::too_many_arguments)]
pub(in crate::autopilot) async fn get_or_create_experiment_design(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    intervention_key: &str,
    logical_cycle_key: &str,
    unit_kind: crowdrelay_brain::ExperimentUnitKind,
    eligible_units: Vec<String>,
    holdout_probability: f64,
    strategy: &str,
    min_eligible_units: u32,
    min_expected_control: u32,
    min_expected_treatment: u32,
    now: time::OffsetDateTime,
) -> Result<crowdrelay_brain::ExperimentDesign, RepositoryError> {
    let pool = &repo.pool;
    let interference_policy =
        crowdrelay_brain::InterferencePolicy::from_unit_and_template(unit_kind, intervention_key);
    let estimand = serde_json::json!({
        "intervention": intervention_key,
        "strategy": strategy,
        "unit_kind": unit_kind.as_str(),
        "population_size": eligible_units.len(),
    });
    let eligibility_criteria = serde_json::json!({
        "is_direct_action": true,
        "template_id": intervention_key,
        // portfolio_selected is NOT recorded here because the
        // experiment is designed BEFORE portfolio selection.
        // The population is "all eligible candidates", not
        // "portfolio-selected candidates".
    });
    let selection_context = serde_json::json!({
        "holdout_probability": holdout_probability,
        "strategy": strategy,
    });
    let eligible_json = serde_json::to_value(&eligible_units).unwrap_or(serde_json::json!([]));

    // P1-a: Try INSERT first. ON CONFLICT DO NOTHING returns no row on retry.
    // This lets us distinguish first creation (row returned) from retry
    // (no row → SELECT the persisted design).
    let insert_row = sqlx::query(
        r#"
        INSERT INTO viryaos_experiment_designs
            (workspace_id, intervention_key, logical_cycle_key, unit_kind,
             holdout_probability, interference_policy, eligible_units,
             estimand, eligibility_criteria, selection_context,
             designed_at, strategy)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (workspace_id, intervention_key, logical_cycle_key)
        DO NOTHING
        RETURNING experiment_uuid, assignment_round, experiment_status,
                  expected_treatment_count, expected_control_count,
                  holdout_probability, interference_policy, unit_kind,
                  eligible_units, estimand, eligibility_criteria,
                  selection_context, strategy, designed_at
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(intervention_key)
    .bind(logical_cycle_key)
    .bind(unit_kind.as_str())
    .bind(holdout_probability)
    .bind(interference_policy.as_str())
    .bind(&eligible_json)
    .bind(&estimand)
    .bind(&eligibility_criteria)
    .bind(&selection_context)
    .bind(now)
    .bind(strategy)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;

    use sqlx::Row;

    // P1-a: On first creation, run check_power and persist the results.
    // On retry, SELECT the full persisted design and return it as-is.
    let row = if let Some(row) = insert_row {
        // First creation — run the power check and persist results.
        let experiment_uuid: uuid::Uuid = row.try_get("experiment_uuid").unwrap_or_default();
        let assignment_round: i32 = row.try_get("assignment_round").unwrap_or(1);
        let status_str: String = row
            .try_get("experiment_status")
            .unwrap_or_else(|_| "active".to_string());
        let expected_treatment: Option<i32> =
            row.try_get("expected_treatment_count").ok().flatten();
        let expected_control: Option<i32> = row.try_get("expected_control_count").ok().flatten();
        let mut design = crowdrelay_brain::ExperimentDesign {
            experiment_uuid,
            assignment_round: assignment_round as u32,
            intervention_key: intervention_key.to_owned(),
            logical_cycle_key: logical_cycle_key.to_owned(),
            unit_kind,
            eligible_units,
            estimand,
            interference_policy,
            assigned_at: now,
            holdout_probability,
            eligibility_criteria,
            selection_context,
            experiment_status: crowdrelay_brain::ExperimentStatus::parse(&status_str)
                .unwrap_or(crowdrelay_brain::ExperimentStatus::Active),
            expected_treatment_count: expected_treatment.map(|v| v as u32),
            expected_control_count: expected_control.map(|v| v as u32),
        };

        // P0-4: Run the power check on first creation only and persist.
        let computed_status = design.check_power(
            min_eligible_units,
            min_expected_control,
            min_expected_treatment,
        );
        let computed_status_str = computed_status.as_str();
        sqlx::query(
            r#"
            UPDATE viryaos_experiment_designs
            SET experiment_status = $2,
                expected_treatment_count = $3,
                expected_control_count = $4
            WHERE experiment_uuid = $1
            "#,
        )
        .bind(experiment_uuid)
        .bind(computed_status_str)
        .bind(design.expected_treatment_count.map(|v| v as i32))
        .bind(design.expected_control_count.map(|v| v as i32))
        .execute(pool)
        .await
        .map_err(map_sqlx)?;

        return Ok(design);
    } else {
        // P1-a: Retry — SELECT the full persisted design. Do NOT reconstruct
        // from caller inputs. Do NOT re-run check_power. The design is
        // immutable after creation.
        sqlx::query(
            r#"
            SELECT experiment_uuid, assignment_round, experiment_status,
                   expected_treatment_count, expected_control_count,
                   holdout_probability, interference_policy, unit_kind,
                   eligible_units, estimand, eligibility_criteria,
                   selection_context, strategy, designed_at
            FROM viryaos_experiment_designs
            WHERE workspace_id = $1
              AND intervention_key = $2
              AND logical_cycle_key = $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(intervention_key)
        .bind(logical_cycle_key)
        .fetch_one(pool)
        .await
        .map_err(map_sqlx)?
    };

    // P1-a: Construct the design from persisted columns only.
    let experiment_uuid: uuid::Uuid = row.try_get("experiment_uuid").unwrap_or_default();
    let assignment_round: i32 = row.try_get("assignment_round").unwrap_or(1);
    let status_str: String = row
        .try_get("experiment_status")
        .unwrap_or_else(|_| "active".to_string());
    let expected_treatment: Option<i32> = row.try_get("expected_treatment_count").ok().flatten();
    let expected_control: Option<i32> = row.try_get("expected_control_count").ok().flatten();
    let persisted_holdout: f64 = row
        .try_get("holdout_probability")
        .unwrap_or(holdout_probability);
    let persisted_interference_str: String = row
        .try_get("interference_policy")
        .unwrap_or_else(|_| interference_policy.as_str().to_string());
    let persisted_unit_kind_str: String = row
        .try_get("unit_kind")
        .unwrap_or_else(|_| unit_kind.as_str().to_string());
    let persisted_eligible: serde_json::Value = row
        .try_get("eligible_units")
        .unwrap_or(serde_json::json!([]));
    let persisted_estimand: serde_json::Value = row.try_get("estimand").unwrap_or(estimand);
    let persisted_eligibility: serde_json::Value = row
        .try_get("eligibility_criteria")
        .unwrap_or(eligibility_criteria);
    let persisted_selection: serde_json::Value = row
        .try_get("selection_context")
        .unwrap_or(selection_context);
    let designed_at: time::OffsetDateTime = row.try_get("designed_at").unwrap_or(now);

    let persisted_unit_kind =
        crowdrelay_brain::ExperimentUnitKind::parse(&persisted_unit_kind_str).unwrap_or(unit_kind);
    let persisted_interference =
        crowdrelay_brain::InterferencePolicy::parse(&persisted_interference_str)
            .unwrap_or(interference_policy);
    let persisted_eligible_units: Vec<String> =
        serde_json::from_value(persisted_eligible).unwrap_or(eligible_units);

    Ok(crowdrelay_brain::ExperimentDesign {
        experiment_uuid,
        assignment_round: assignment_round as u32,
        intervention_key: intervention_key.to_owned(),
        logical_cycle_key: logical_cycle_key.to_owned(),
        unit_kind: persisted_unit_kind,
        eligible_units: persisted_eligible_units,
        estimand: persisted_estimand,
        interference_policy: persisted_interference,
        assigned_at: designed_at,
        holdout_probability: persisted_holdout,
        eligibility_criteria: persisted_eligibility,
        selection_context: persisted_selection,
        experiment_status: crowdrelay_brain::ExperimentStatus::parse(&status_str)
            .unwrap_or(crowdrelay_brain::ExperimentStatus::Active),
        expected_treatment_count: expected_treatment.map(|v| v as u32),
        expected_control_count: expected_control.map(|v| v as u32),
    })
}

/// Records a first-class experiment assignment in the
/// `viryaos_experiment_assignments` table. The experimental unit is
/// explicitly defined — not always workspace-wide. When interference is
/// not controllable, the `experiment_kind` is downgraded to
/// `matched_quasi_experiment`.
///
/// P1-b: The assignment is get-or-create by logical key
/// `(workspace_id, experiment_uuid, assignment_round, unit_id)`. A retry
/// with the same logical key retrieves the existing assignment — no
/// duplicate rows, no unique-constraint violation. The deterministic
/// `assignment_id` (derived from the logical key) ensures the PK is
/// stable across retries.
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

    // P0-2: Use a transaction so that control assignment + control evidence
    // are atomic. A crash between them is now impossible — they commit
    // together or not at all.
    let mut tx = pool.begin().await.map_err(map_sqlx)?;

    // INSERT assignment — RETURNING id lets us detect first creation vs
    // retry. On retry (ON CONFLICT DO NOTHING), no row is returned and we
    // skip evidence creation to prevent duplicates.
    let inserted = sqlx::query(
        r#"
        INSERT INTO viryaos_experiment_assignments
            (id, workspace_id, unit_id, unit_kind, arm, assigned_at,
             propensity, intended_holdout_probability, intended_template_id,
             context, prediction, action_id, strategy, experiment_kind,
             contamination_estimate, is_interference_controllable,
             experiment_uuid, assignment_round,
             eligibility_criteria, selection_context,
             interference_policy, assignment_time_contamination,
             experiment_status, execution_status)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                $17, $18, $19, $20, $21, $22, $23, $24)
        ON CONFLICT (workspace_id, experiment_uuid, assignment_round, unit_id)
        DO NOTHING
        RETURNING id
        "#,
    )
    .bind(&assignment.assignment_id)
    .bind(workspace_id.into_uuid())
    .bind(&assignment.unit_id)
    .bind(assignment.unit_kind.as_str())
    .bind(assignment.arm.as_str())
    .bind(assignment.assigned_at)
    .bind(assignment.propensity)
    .bind(assignment.intended_holdout_probability)
    .bind(&assignment.intended_template_id)
    .bind(&context_json)
    .bind(&prediction_json)
    .bind(assignment.action_id)
    .bind(strategy)
    .bind(kind.as_str())
    .bind(assignment.interference_score)
    .bind(assignment.is_interference_controllable)
    .bind(assignment.experiment_uuid)
    .bind(assignment.assignment_round as i32)
    .bind(&assignment.eligibility_criteria)
    .bind(&assignment.selection_context)
    .bind(assignment.interference_policy.as_str())
    .bind(assignment.interference_score)
    .bind(assignment.experiment_status.as_str())
    .bind(assignment.execution_status.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx)?;

    // Only record control evidence on FIRST creation (inserted=Some).
    // On retry (inserted=None), the evidence already exists — skip to
    // prevent duplicates. The evidence INSERT is in the same transaction
    // as the assignment INSERT — atomic.
    if assignment.arm == crowdrelay_brain::TreatmentAssignment::Control && inserted.is_some() {
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
            assignment.action_id,
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
        super::evidence::record_growth_evidence_in_tx(&mut tx, workspace_id, &evidence).await?;
    }
    tx.commit().await.map_err(map_sqlx)?;
    Ok(())
}

/// Transitions the execution_status of an experiment assignment.
///
/// Monotonic: only `executed → failed` is allowed. All other transitions
/// are silently no-ops (the WHERE clause prevents them). This is the
/// one transition point from the executor/result path.
///
/// Retry-safe: setting the same status is a no-op (idempotent).
pub(in crate::autopilot) async fn update_execution_status(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    assignment_id: &str,
    new_status: crowdrelay_brain::ExecutionStatus,
) -> Result<(), RepositoryError> {
    // Only executed → failed is allowed. The WHERE clause enforces this
    // at the DB level — no application-level race condition possible.
    sqlx::query(
        r#"
        UPDATE viryaos_experiment_assignments
        SET execution_status = $3
        WHERE workspace_id = $1
          AND id = $2
          AND execution_status = 'executed'
          AND $3 = 'failed'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(assignment_id)
    .bind(new_status.as_str())
    .execute(&repo.pool)
    .await
    .map_err(map_sqlx)?;
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
