// Walk-forward validation and the hypothesis lifecycle — extracted from
// growth_intelligence_context.rs to keep both inside the source-size ratchet.
//
// One job: judge each worker template against the outcomes its own dispatches
// produced, and degrade the ones whose edge did not survive out of sample.
// Nothing here reaches candidate generation; the cycle reads the resulting
// `hypothesis_state` off the snapshots afterwards.
impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    /// Validates every template against its own resolved evidence and
    /// persists any lifecycle transition that validation justifies.
    ///
    /// Mutates `snapshots` in place so the rest of the cycle sees the state it
    /// will act under, rather than the one loaded before validation ran.
    async fn validate_hypotheses(
        &self,
        snapshots: &mut [crowdrelay_brain::GrowthIntelligenceSnapshot],
    ) -> Result<(), AutopilotError> {
        // Walk-forward validation: load resolved evidence and validate
        // each template's out-of-sample performance. Templates that fail
        // validation are degraded from Active to Degraded, reducing
        // their dispatch budget. Templates that pass are promoted
        // toward Active. This is the wiring point between the evidence
        // persistence layer and the hypothesis lifecycle.
        //
        // The validation runs on treatment evidence only (control rows
        // have no observed outcome). The purge gap is 16 days to account
        // for Y30 durability overlap.
        let growth_evidence = self
            .repository
            .load_growth_evidence(self.workspace_id, None)
            .await?;
        // Grouped once, by moving each row into the bucket its opportunity
        // names. The loop below used to re-scan the whole batch per template
        // and clone every match, so a workspace with T templates and E
        // resolved rows cloned O(T·E) times every five minutes, forever,
        // against a table that only grows.
        //
        // The key is the first segment of `template:target:action:hash`, not a
        // prefix of it. `starts_with(template_id)` matched any template whose
        // id begins with another's — no pair collides today, and the first
        // `social-post-v2` beside `social-post` would have silently validated
        // one template against the other's outcomes.
        let mut evidence_by_template: std::collections::HashMap<
            String,
            Vec<crowdrelay_brain::GrowthEvidence>,
        > = std::collections::HashMap::new();
        for evidence in growth_evidence {
            let Some(template) = evidence
                .opportunity_id
                .as_deref()
                .and_then(|id| id.split(':').next())
                .filter(|template| !template.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            evidence_by_template.entry(template).or_default().push(evidence);
        }
        const NO_EVIDENCE: &[crowdrelay_brain::GrowthEvidence] = &[];
        for snapshot in snapshots.iter_mut() {
            let template_evidence = evidence_by_template
                .get(&snapshot.template_id)
                .map_or(NO_EVIDENCE, Vec::as_slice);
            let result =
                crowdrelay_brain::validation::validate_evidence_for_promotion(template_evidence);
            // Only adjust the hypothesis state if we have enough evidence
            // to validate meaningfully (OOS observations >= 5). Below
            // that, the default Active state is preserved.
            if result.out_of_sample.observations >= 5 && !result.passed {
                // Failed validation — degrade to Degraded (quarter budget).
                // Persist the transition so the next cycle loads the
                // degraded state instead of resetting to Active.
                let new_state = crowdrelay_brain::hypothesis::HypothesisState::Degraded;
                let previous_state = snapshot.hypothesis_state;
                if previous_state != new_state {
                    snapshot.hypothesis_state = new_state;
                    let saved = self
                        .repository
                        .save_hypothesis_state(
                            self.workspace_id,
                            &snapshot.template_id,
                            new_state,
                        )
                        .await;
                    // Only record the revision once the transition is durable.
                    // A ledger entry for a state the next cycle will not load
                    // describes learning that did not survive the cycle.
                    if saved.is_ok() {
                        self.record_hypothesis_revision(
                            &snapshot.template_id,
                            previous_state,
                            new_state,
                            &result,
                            template_evidence,
                        )
                        .await;
                    }
                }
            }
            // Passed validation (or insufficient evidence) — keep at Active
        }
        Ok(())
    }

    /// Records a hypothesis lifecycle transition in the belief-revision
    /// ledger, citing the dispatches whose measured outcomes failed
    /// validation.
    ///
    /// Best-effort by design: the transition is already persisted when this
    /// runs, so a failure here costs the operator the explanation and costs
    /// the brain nothing.
    async fn record_hypothesis_revision(
        &self,
        template_id: &str,
        previous: crowdrelay_brain::hypothesis::HypothesisState,
        current: crowdrelay_brain::hypothesis::HypothesisState,
        result: &crowdrelay_brain::validation::WalkForwardResult,
        template_evidence: &[crowdrelay_brain::GrowthEvidence],
    ) {
        let caused_by: Vec<Uuid> = template_evidence
            .iter()
            .filter_map(|evidence| evidence.action_id)
            .collect();
        let Some(revision) = crate::autopilot::hypothesis_state_revision(
            template_id,
            previous,
            current,
            result.out_of_sample.observations,
            &caused_by,
        ) else {
            return;
        };
        let _ = self
            .repository
            .record_belief_revisions(self.workspace_id, std::slice::from_ref(&revision))
            .await;
    }
}
