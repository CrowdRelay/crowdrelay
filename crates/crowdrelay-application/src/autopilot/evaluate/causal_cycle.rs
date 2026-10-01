// One causal-model snapshot per autopilot cycle.
//
// Loading the model replays delta evidence and advances stored strategy state.
// That is learning work, not a pure getter, so calling it once per context
// would double-apply the same evidence. The cycle owns the read and checkpoint;
// consumers only borrow the resulting decision-time belief state.

impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    async fn load_cycle_causal_model(
        &self,
        policies: &[AutopilotPolicy],
        report: &mut AutopilotCycleReport,
    ) -> Result<Option<LoadedCausalModel>, AutopilotError> {
        let growth_intelligence_enabled = policies
            .iter()
            .any(|policy| policy.enabled && policy.context == AutopilotContext::GrowthIntelligence);
        let beacon_enabled = policies
            .iter()
            .any(|policy| policy.enabled && policy.context == AutopilotContext::Beacon);

        if growth_intelligence_enabled {
            // Preserve GI's existing semantics: running that context without
            // the model would silently change what the Brain means.
            return self
                .repository
                .load_causal_model(self.workspace_id)
                .await
                .map(Some)
                .map_err(AutopilotError::from);
        }

        if !beacon_enabled {
            return Ok(None);
        }

        // Beacon learning is advisory ordering only. The relationship policy
        // remains useful if learning state is temporarily unreadable, so fall
        // back to the repository's deterministic order rather than suppressing
        // due human-reviewed asks.
        match self.repository.load_causal_model(self.workspace_id).await {
            Ok(model) => Ok(Some(model)),
            Err(_) => {
                report.gi_dispatch_log.push(
                    "beacon North-Star ranking unavailable; preserving repository order".into(),
                );
                Ok(None)
            }
        }
    }

    async fn checkpoint_cycle_causal_model(
        &self,
        loaded_model: Option<&LoadedCausalModel>,
        report: &mut AutopilotCycleReport,
    ) {
        let Some(loaded_model) = loaded_model else {
            return;
        };
        if self
            .repository
            .save_brain_state_checkpoint(self.workspace_id, &loaded_model.model)
            .await
            .is_err()
        {
            report.gi_dispatch_log.push(
                "causal model checkpoint failed; learning will retry from the previous cursor"
                    .into(),
            );
        }
    }
}
