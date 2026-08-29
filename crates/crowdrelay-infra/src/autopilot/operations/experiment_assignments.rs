//! Persistence for first-class experiment assignments.
//!
//! The experimental unit is explicitly defined — not always workspace-wide.
//! When interference is not controllable, the `experiment_kind` is
//! downgraded to `matched_quasi_experiment`.

use crowdrelay_domain::WorkspaceId;

use super::{PostgresAutopilotRepository, RepositoryError, map_sqlx};

/// Records a first-class experiment assignment in the
/// `viryaos_experiment_assignments` table. The experimental unit is
/// explicitly defined — not always workspace-wide. When interference
/// is not controllable, the `experiment_kind` is downgraded to
/// `matched_quasi_experiment`.
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
             contamination_estimate, is_interference_controllable)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(&assignment.experiment_id)
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
    .execute(pool)
    .await
    .map_err(map_sqlx)?;

    // Also record a control evidence row when this is a control arm,
    // so the measurement system can measure the control group's fan
    // growth. The evidence row uses the experiment_id as the linking
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
