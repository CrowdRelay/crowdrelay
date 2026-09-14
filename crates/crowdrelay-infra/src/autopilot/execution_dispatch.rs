// Dispatch envelope — the prediction and experiment rows an action needs
// before its outcome can teach anything.
//
// Split out of `execution.rs` when that chunk crossed the 1000-line limit the
// modularity contract sets for one `include!` chunk. Pure relocation: an
// included chunk shares its parent's scope, so nothing here changed but the
// file it lives in.

/// Writes the dispatch-prediction and growth-evidence envelope for an action
/// that never passed through the decision persist — outcome-created actions
/// (an approved engager post, an agent content draft, a signal push) are
/// real fan-facing interventions, and without the envelope their scheduled
/// measurements UPDATE rows that do not exist and the causal model learns
/// nothing from the outcome.
///
/// The prediction is the cold prior (`DEFAULT_EXPECTED_*`), the honest value
/// the model would have returned for an unseen template. The outcome model
/// learns from raw observed counts, not the prediction — so the constant
/// narrows calibration fidelity, never corrupts the learner.
///
/// Both inserts are `ON CONFLICT DO NOTHING`: evaluator-created actions
/// already carry their envelope and this is a no-op for them.
pub(super) async fn ensure_dispatch_envelope(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    payload: &AutopilotActionPayload,
) -> Result<(), RepositoryError> {
    use crowdrelay_brain::{
        DispatchContext, GrowthEvidence, OpportunityAction, OpportunityId, TreatmentAssignment,
        channel_for_template,
    };

    let (template_id, recipient_id, target_key, context) = match payload {
        AutopilotActionPayload::RequestCommunityEngagement {
            target_id,
            subreddit,
            ..
        } => {
            let handle = subreddit
                .clone()
                .unwrap_or_else(|| target_id.to_string());
            let context = DispatchContext {
                post_format: Some("link".to_owned()),
                ..DispatchContext::default()
            };
            (
                "community-engager".to_owned(),
                handle.clone(),
                Some(format!("community:{handle}")),
                context,
            )
        }
        AutopilotActionPayload::RequestAgentContent {
            template_id,
            task_id,
            ..
        } => (
            template_id.clone().unwrap_or_else(|| "agent-content".to_owned()),
            task_id.to_string(),
            None,
            DispatchContext::default(),
        ),
        AutopilotActionPayload::RequestSignalPush { task_id, .. } => (
            "signal-inviter".to_owned(),
            task_id.to_string(),
            None,
            DispatchContext::default(),
        ),
        // Every other kind was created by the decision persist, which wrote
        // its envelope already — nothing to fill.
        _ => return Ok(()),
    };

    let context_json =
        serde_json::to_value(&context).unwrap_or_else(|_| serde_json::json!({}));
    sqlx::query(
        r#"
        INSERT INTO viryaos_dispatch_predictions
            (workspace_id, action_id, template_id,
             expected_new_fans, expected_signal_installs, context)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (action_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(&template_id)
    .bind(crowdrelay_brain::DEFAULT_EXPECTED_FANS)
    .bind(crowdrelay_brain::DEFAULT_EXPECTED_SIGNAL)
    .bind(&context_json)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;

    let target = target_key
        .clone()
        .unwrap_or_else(|| format!("action:{action_id}"));
    let opportunity_id = OpportunityId::new(
        &template_id,
        &target,
        OpportunityAction::Post,
        &context,
    );
    let evidence = GrowthEvidence::at_dispatch(
        workspace_id.into_uuid(),
        Some(action_id.into_uuid()),
        Some(opportunity_id.to_string()),
        recipient_id,
        channel_for_template(&template_id),
        1,
        TreatmentAssignment::Treatment,
        1.0,
        crowdrelay_brain::DEFAULT_EXPECTED_FANS,
        crowdrelay_brain::DEFAULT_EXPECTED_SIGNAL,
        context,
        target_key,
        None,
        None,
        crowdrelay_brain::EvidenceQuality::Observational,
    );
    crate::autopilot::operations::evidence::record_growth_evidence_in_tx(
        transaction,
        workspace_id,
        &evidence,
    )
    .await?;
    Ok(())
}
