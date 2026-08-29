// GrowthIntelligence context arm — extracted from evaluate.rs to keep
// the orchestrator under the modularity contract line limit.
impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    async fn evaluate_growth_intelligence_context(
        &self,
        policy: &AutopilotPolicy,
        now: OffsetDateTime,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
    ) -> Result<(), AutopilotError> {
        let snapshots = self
            .repository
            .load_growth_intelligence_snapshots(self.workspace_id, now)
            .await?;
        // Load the causal model from past predictions + outcomes.
        // The brain uses this to predict how many fans each
        // dispatch will produce, and learns from prediction errors.
        let causal_model = self.repository.load_causal_model(self.workspace_id).await?;
        // Load the strategy posterior from brain state. The brain
        // learns which growth strategies work best in each world
        // state (growth trend × event proximity) via UCB
        // exploration. When data is thin, the fixed rank-based
        // multiplier is used as fallback.
        let strategy_posterior = self
            .repository
            .load_brain_state(self.workspace_id, "strategy_posterior")
            .await
            .ok()
            .flatten()
            .and_then(|(state, _ts)| {
                serde_json::from_value::<
                    crowdrelay_brain::StateConditionedStrategyPosterior,
                >(state)
                .ok()
            })
            .unwrap_or_default();
        // Load the exploration memory from past dispatch
        // predictions. The brain uses this to compute novelty:
        // unexplored (template, context) pairs get an exploration
        // bonus in the EFE score.
        let exploration_memory = self
            .repository
            .load_exploration_memory(self.workspace_id)
            .await
            .unwrap_or_default();
        // Derive the growth strategy from the world model, with
        // hysteresis — the brain doesn't flip-flop between
        // strategies every cycle when conditions are borderline.
        // The previous strategy is inferred from the most
        // recently dispatched template.
        let last_template = self
            .repository
            .load_last_dispatched_template(self.workspace_id)
            .await
            .unwrap_or(None);
        let previous_strategy = last_template
            .as_deref()
            .map(GrowthStrategy::infer_from_template);
        let strategy = if let Some(first) = snapshots.first() {
            GrowthStrategy::from_world_model_with_hysteresis(
                &first.world_model,
                previous_strategy,
            )
        } else {
            GrowthStrategy::default()
        };
        // Collect all unconsumed insight IDs across all snapshots.
        // Pre-allocate: each snapshot typically has 0-3 insights.
        let mut consumed_ids: Vec<Uuid> = Vec::with_capacity(snapshots.len() * 2);
        // Collect all eligible candidates with their EFE scores
        // and strategy ranks, then sort by (strategy_rank,
        // efe_score) so the brain dispatches the best
        // opportunities first. When budget limits kick in,
        // the worst opportunities are the ones that get gated.
        let mut scored_candidates: Vec<ScoredCandidate> =
            Vec::with_capacity(snapshots.len());
        for snapshot in &snapshots {
            for insight in &snapshot.recent_insights {
                consumed_ids.push(insight.outcome_id);
            }
            // Build the enriched dispatch context (same as
            // evaluate_growth_intelligence uses) so the novelty
            // lookup matches the context hash that gets recorded.
            let ctx = build_dispatch_context(snapshot, now);
            let novelty =
                exploration_memory.novelty(&snapshot.template_id, &context_hash(&ctx));
            if let Some((
                candidate,
                prediction,
                efe_score,
                strategy_rank,
                treatment_stats,
            )) = growth_intelligence_candidate(
                snapshot,
                policy,
                self.workspace_id,
                now,
                &causal_model,
                strategy,
                novelty,
                &strategy_posterior,
            )? {
                scored_candidates.push((
                    candidate,
                    prediction,
                    efe_score,
                    strategy_rank,
                    treatment_stats,
                ));
            }
        }
        // Sort by EFE score only (lower EFE = better).
        // Strategy alignment no longer modifies the EFE score — the
        // strategy multiplier was removed because it was a second
        // prediction model sitting on top of the causal model. The
        // causal model is now the sole authority for expected fans.
        // Strategy enters via `strategy_rank` for candidate eligibility
        // and exploration allocation, not as a score modifier.
        scored_candidates
            .sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        // Extract the GI policy for resource costs and holdout config.
        // When the autopilot isn't in GrowthIntelligence mode, use the
        // default GI policy (which has sensible default costs).
        let gi_policy = match &policy.config {
            AutopilotPolicyConfig::GrowthIntelligence(gi) => gi.clone(),
            _ => GrowthIntelligencePolicy::default(),
        };
        // Run the portfolio optimizer to select the optimal set
        // of candidates, accounting for audience overlap, fatigue,
        // and resource costs. See `evaluate/portfolio.rs` for the
        // selection logic.
        let pending_measurement_count = self
            .repository
            .count_pending_measurements(self.workspace_id)
            .await
            .unwrap_or(0);
        let selection = portfolio::select_portfolio(
            &scored_candidates,
            &gi_policy,
            pending_measurement_count,
        );
        let selected_keys = portfolio::selected_keys(&selection);
        // Extract the holdout probability from the policy.
        // Guardrails: clamped to [0.0, 0.10] — 0% = off, max 10%.
        let holdout_probability = gi_policy.randomized_holdout_probability.clamp(0.0, 0.10);
        let mut dispatched_count = 0usize;
        for (candidate, prediction, _efe, _rank, _stats) in scored_candidates {
            // Skip candidates that the portfolio optimizer
            // rejected (negative marginal value, audience
            // overlap, budget exhausted, or superseded).
            // When do_nothing is true, ALL candidates had
            // negative marginal value — skip everything.
            if selection.do_nothing || !selected_keys.contains(&candidate.decision_key)
            {
                continue;
            }
            // Randomized holdout via first-class ExperimentAssignment.
            // With probability `holdout_probability`, skip the dispatch
            // and record a control-arm ExperimentAssignment instead.
            // This produces real causal evidence (randomized holdout)
            // for high-volume actions. Only applies to direct-action
            // workers — scanner/strategist are never held out.
            //
            // The experimental unit is the target community (the
            // decision_key), NOT the workspace. This is a first-class
            // experiment assignment — not a synthetic action_id.
            let template_id = match &candidate.action {
                AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
                    template_id.as_str()
                }
                _ => "",
            };
            let is_direct_action = !template_id.is_empty()
                && !matches!(template_id, "reddit-scanner" | "growth-strategist");
            // Deterministic pseudo-random from the decision key
            // — the same decision key always gets the same
            // holdout assignment within one cycle, preventing
            // flapping. The hash is mapped to [0, 1) and
            // compared against the holdout probability.
            let holdout_roll = deterministic_roll(&candidate.decision_key);
            if holdout_probability > 0.0
                && is_direct_action
                && holdout_roll < holdout_probability
            {
                // Control arm: record an ExperimentAssignment with
                // arm=Control. No action is dispatched — the worker
                // never runs. The measurement system will measure
                // the workspace's fan growth in the 14-day window,
                // which is the control group's counterfactual.
                //
                // The unit is the target community (decision_key),
                // which is isolatable — other communities can still
                // receive treatment without contaminating this
                // control.
                let assignment = crowdrelay_brain::ExperimentAssignment {
                    experiment_id: format!(
                        "exp:{template_id}:{}",
                        candidate.decision_key
                    ),
                    candidate_id: candidate.decision_key.clone(),
                    unit_id: candidate.decision_key.clone(),
                    unit_kind: crowdrelay_brain::ExperimentUnitKind::TargetCommunity,
                    arm: crowdrelay_brain::TreatmentAssignment::Control,
                    assigned_at: now,
                    propensity: 1.0 - holdout_probability,
                    intended_template_id: template_id.to_owned(),
                    context: prediction.context.clone(),
                    prediction: prediction.clone(),
                    action_id: None,
                    contamination_estimate: 0.0,
                    is_interference_controllable: true,
                };
                let _ = self
                    .repository
                    .record_experiment_assignment(
                        self.workspace_id,
                        &assignment,
                        Some(strategy.as_str()),
                    )
                    .await;
                continue;
            }
            let persisted = self.persist(&candidate, limits, report).await?;
            // Record the prediction for the dopamine loop. Pass the actual
            // strategy so the strategy posterior learns from the real strategy.
            if let Some(action_id) = persisted {
                let _ = self
                    .repository
                    .record_dispatch_prediction(
                        self.workspace_id,
                        action_id,
                        &prediction,
                        Some(strategy.as_str()),
                        holdout_probability,
                    )
                    .await;
                // Record the treatment-arm ExperimentAssignment. This
                // pairs with the control arm above to produce real
                // causal evidence. The unit is the target community
                // (decision_key), same as the control arm.
                let treatment_assignment = crowdrelay_brain::ExperimentAssignment {
                    experiment_id: format!(
                        "exp:{template_id}:{}",
                        candidate.decision_key
                    ),
                    candidate_id: candidate.decision_key.clone(),
                    unit_id: candidate.decision_key.clone(),
                    unit_kind: crowdrelay_brain::ExperimentUnitKind::TargetCommunity,
                    arm: crowdrelay_brain::TreatmentAssignment::Treatment,
                    assigned_at: now,
                    propensity: 1.0 - holdout_probability,
                    intended_template_id: template_id.to_owned(),
                    context: prediction.context.clone(),
                    prediction: prediction.clone(),
                    action_id: Some(action_id),
                    contamination_estimate: 0.0,
                    is_interference_controllable: true,
                };
                let _ = self
                    .repository
                    .record_experiment_assignment(
                        self.workspace_id,
                        &treatment_assignment,
                        Some(strategy.as_str()),
                    )
                    .await;
            }
            dispatched_count += 1;
        }
        // Only mark insights as consumed when the brain actually
        // acted on them (at least one dispatch was produced).
        // When do_nothing is true, the brain chose not to act —
        // keeping insights unconsumed lets them be re-evaluated
        // next cycle with potentially different context.
        if !consumed_ids.is_empty() && dispatched_count > 0 {
            let _ = self
                .repository
                .mark_insights_consumed(self.workspace_id, &consumed_ids)
                .await;
        }
        // Save the causal model checkpoint for fast startup
        // with delta replay on the next cycle. This is
        // best-effort — a failed checkpoint just means the
        // next cycle does a full replay.
        let _ = self
            .repository
            .save_brain_state_checkpoint(self.workspace_id, &causal_model)
            .await;
        // Save the strategy posterior checkpoint. Best-effort —
        // a failed save just means the next cycle starts fresh.
        if let Ok(state) = serde_json::to_value(&strategy_posterior) {
            let _ = self
                .repository
                .save_brain_state(self.workspace_id, "strategy_posterior", &state)
                .await;
        }
        Ok(())
    }
}
