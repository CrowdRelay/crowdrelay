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
            .load_brain_state(self.workspace_id, "strategy_learner")
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
        // Strategy alignment is already baked into the EFE score
        // via a strategy multiplier — the evaluator applies a
        // bonus to strategy-aligned templates rather than using
        // strategy as a primary sort key. This ensures the North
        // Star (incremental fans via EFE) is never overridden by
        // strategy preference.
        scored_candidates
            .sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        // Run the portfolio optimizer to select the optimal set
        // of candidates, accounting for audience overlap and
        // fatigue. See `evaluate/portfolio.rs` for the selection
        // logic.
        let selection = portfolio::select_portfolio(&scored_candidates);
        let selected_keys = portfolio::selected_keys(&selection);
        // Extract the holdout probability from the policy.
        // Guardrails: clamped to [0.0, 0.10] — 0% = off, max 10%.
        let holdout_probability = match &policy.config {
            AutopilotPolicyConfig::GrowthIntelligence(gi) => {
                gi.randomized_holdout_probability.clamp(0.0, 0.10)
            }
            _ => 0.0,
        };
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
            // Randomized holdout: with probability
            // `holdout_probability`, skip the dispatch and
            // record a control-group evidence row instead.
            // This produces gold-standard causal evidence
            // (randomized holdout) for high-volume actions.
            // Only applies to direct-action workers —
            // scanner/strategist are never held out.
            let template_id = match &candidate.action {
                AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
                    template_id.as_str()
                }
                _ => "",
            };
            let is_direct_action =
                !matches!(template_id, "reddit-scanner" | "growth-strategist");
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
                // Holdout fired: record a control-group
                // evidence row with a synthetic action_id.
                // No action is dispatched — the worker never
                // runs. The measurement system will measure
                // the workspace's fan growth in the 14-day
                // window, which is the control group's
                // counterfactual.
                let holdout_action_id = Uuid::now_v7();
                let _ = self
                    .repository
                    .record_holdout_control(
                        self.workspace_id,
                        holdout_action_id,
                        &prediction,
                        Some(strategy.as_str()),
                        holdout_probability,
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
                .save_brain_state(self.workspace_id, "strategy_learner", &state)
                .await;
        }
        Ok(())
    }
}
