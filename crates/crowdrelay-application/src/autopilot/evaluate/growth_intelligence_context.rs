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
        // exploration. The strategy posterior influences candidate
        // eligibility and exploration allocation only — it never
        // modifies predicted fan value, treatment effect, or
        // DecisionValue.
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
            // P0-3: community-engager now returns one candidate per
            // target community. Other templates return 0 or 1.
            let candidates = growth_intelligence_candidate(
                snapshot,
                policy,
                self.workspace_id,
                now,
                &causal_model,
                strategy,
                novelty,
                &strategy_posterior,
            )?;
            scored_candidates.extend(candidates);
        }
        // Sort by EFE score (lower EFE = better) for candidate POOL
        // ORDERING only. This determines which candidates enter the
        // portfolio pool first — it does NOT determine which candidates
        // WIN. The portfolio optimizer makes the final selection using
        // DecisionValue.total() as the sole ranking authority.
        //
        // EFE decides what is worth learning about (candidate generation).
        // DecisionValue decides what is worth doing (portfolio ranking).
        // The optimizer must never combine EFE with DecisionValue.total().
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
        // ── Experiment Design Engine ──
        //
        // EXPERIMENT DESIGN ≠ CANDIDATE DISPATCH. The design is created
        // FIRST, then units are assigned to treatment/control arms within
        // that design. This ensures treatment and control are genuinely
        // arms of the same experiment, not separate experiments that
        // happen to share a label.
        //
        // P0-1: The design is persisted via get_or_create_experiment_design.
        // The same (workspace, intervention, logical_cycle_key) always
        // converges on the same experiment_uuid. Retries and concurrent
        // evaluators reuse the same design — no more fresh UUIDs per run.
        //
        // P0-3: community-engager uses TargetCommunity units. Each community
        // is a distinct experimental unit with its own assignment. Other
        // templates use Workspace units (quasi-experimental only).
        //
        // P0-4: Before assignment, the design is checked for statistical
        // power. If the eligible population is too small, the experiment
        // is marked InsufficientPower and candidates execute observationally.
        let mut dispatched_count = 0usize;
        // Group selected candidates by intervention (template_id).
        // Each group becomes one ExperimentDesign with its own UUID.
        // Use an IndexMap-like approach: preserve insertion order so
        // the first-seen template gets dispatched first.
        let mut experiment_groups: Vec<(
            String,                               // template_id (intervention key)
            Vec<(DecisionCandidate, DispatchPrediction)>, // candidates in this group
        )> = Vec::new();
        for (candidate, prediction, _efe, _rank, _stats) in &scored_candidates {
            if selection.do_nothing || !selected_keys.contains(&candidate.decision_key) {
                continue;
            }
            let template_id = match &candidate.action {
                AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
                    template_id.as_str()
                }
                _ => "",
            };
            // Only direct-action workers participate in experiments.
            // Scanner/strategist are never held out — they are
            // workspace-wide intelligence gathering, not community-
            // targeted treatments.
            let is_direct_action = !template_id.is_empty()
                && !matches!(template_id, "reddit-scanner" | "growth-strategist");
            if !is_direct_action {
                // Dispatch directly without experiment assignment.
                // These are intelligence-gathering workers, not
                // community-targeted treatments.
                let persisted = self.persist(candidate, limits, report).await?;
                if let Some(action_id) = persisted {
                    let _ = self
                        .repository
                        .record_dispatch_prediction(
                            self.workspace_id,
                            action_id,
                            prediction,
                            Some(strategy.as_str()),
                            0.0,
                        )
                        .await;
                }
                dispatched_count += 1;
                continue;
            }
            // Add to the experiment group for this intervention.
            if let Some(group) = experiment_groups.iter_mut().find(|(t, _)| t == template_id) {
                group.1.push((candidate.clone(), prediction.clone()));
            } else {
                experiment_groups.push((
                    template_id.to_owned(),
                    vec![(candidate.clone(), prediction.clone())],
                ));
            }
        }
        // For each intervention group, get-or-create the persisted
        // ExperimentDesign and assign arms to all units within it.
        for (template_id, group_candidates) in &experiment_groups {
            // P0-3: community-engager uses TargetCommunity units.
            // Each community is a distinct experimental unit. Other
            // direct-action templates use Workspace units.
            let unit_kind = if template_id == "community-engager" {
                crowdrelay_brain::ExperimentUnitKind::TargetCommunity
            } else {
                crowdrelay_brain::ExperimentUnitKind::Workspace
            };
            // P0-3: for community-engager, the eligible units are the
            // target communities (subreddits). For workspace-wide
            // templates, the eligible unit is the workspace itself.
            let eligible_units: Vec<String> = if template_id == "community-engager" {
                group_candidates
                    .iter()
                    .map(|(c, _)| unit_id_from_decision_key(&c.decision_key))
                    .collect()
            } else {
                group_candidates.iter().map(|(c, _)| c.decision_key.clone()).collect()
            };
            // P0-1: compute the logical_cycle_key from the cooldown
            // window bucket. This is the same bucketing used in
            // decision_key, so retry within the same cooldown window
            // converges on the same experiment.
            let key_window_hours = key_window_for_template(&gi_policy, template_id);
            let logical_cycle_key = cooldown_window(now, key_window_hours).to_string();
            // P0-1: get-or-create the persisted experiment design.
            // The DB unique index on (workspace, intervention,
            // logical_cycle_key) is the convergence guarantee.
            let mut design = self
                .repository
                .get_or_create_experiment_design(
                    self.workspace_id,
                    template_id,
                    &logical_cycle_key,
                    unit_kind,
                    eligible_units.clone(),
                    holdout_probability,
                    strategy.as_str(),
                    gi_policy.min_eligible_units_for_experiment,
                    gi_policy.min_expected_control_units,
                    gi_policy.min_expected_treatment_units,
                    now,
                )
                .await?;
            // P0-4: The power check is performed inside
            // get_or_create_experiment_design and persisted to the DB.
            // The design's experiment_status already reflects the result.
            let is_insufficient_power =
                design.experiment_status == crowdrelay_brain::ExperimentStatus::InsufficientPower;
            // When power is insufficient, holdout_probability is
            // effectively 0 — all candidates are treatment, evidence
            // is observational. The North Star action is not sacrificed.
            // We also zero the design's holdout_probability so that
            // ExperimentAssignment::from_design computes propensity = 1.0
            // (all treatment), not 1.0 - original_holdout.
            if is_insufficient_power {
                design.holdout_probability = 0.0;
            }
            let effective_holdout = if is_insufficient_power {
                0.0
            } else {
                holdout_probability
            };
            for (candidate, prediction) in group_candidates {
                let unit_id = if template_id == "community-engager" {
                    unit_id_from_decision_key(&candidate.decision_key)
                } else {
                    candidate.decision_key.clone()
                };
                // Per-unit deterministic roll using the experiment_uuid
                // as the randomization seed. The same unit gets the
                // same assignment within one experiment (deterministic
                // after persistence), but different experiments produce
                // different rolls (real randomization, not permanent
                // fate).
                let roll = deterministic_roll(&format!(
                    "{}:{}:{}:{}",
                    design.experiment_uuid, unit_id, design.assignment_round, template_id
                ));
                let is_control = effective_holdout > 0.0 && roll < effective_holdout;
                if is_control {
                    // Control arm: record an ExperimentAssignment with
                    // arm=Control. No action is dispatched — the worker
                    // never runs. The measurement system will measure
                    // the unit's fan growth in the 14-day window
                    // via workspace-level DiD (or community-level
                    // provenance if available), which is the control
                    // group's counterfactual outcome.
                    let assignment = crowdrelay_brain::ExperimentAssignment::from_design(
                        &design,
                        &unit_id,
                        &unit_id,
                        crowdrelay_brain::TreatmentAssignment::Control,
                        prediction,
                        None,
                    );
                    // P0-2: control assignment errors are propagated,
                    // not discarded. A failed control assignment means
                    // the experiment bookkeeping is broken — the cycle
                    // fails rather than silently dropping the assignment.
                    self.repository
                        .record_experiment_assignment(
                            self.workspace_id,
                            &assignment,
                            Some(strategy.as_str()),
                        )
                        .await?;
                    continue;
                }
                // Treatment arm: P0-2 — atomically persist the action
                // AND the experiment assignment in one transaction.
                // ACTION EXISTS ↔ ASSIGNMENT EXISTS ↔ EXECUTION INTENT.
                // No state where the action succeeded but the causal
                // bookkeeping vanished.
                let treatment_assignment =
                    crowdrelay_brain::ExperimentAssignment::from_design(
                        &design,
                        &unit_id,
                        &unit_id,
                        crowdrelay_brain::TreatmentAssignment::Treatment,
                        prediction,
                        None, // action_id filled in by the atomic persist
                    );
                let persisted = self
                    .repository
                    .persist_treatment_with_assignment(
                        self.workspace_id,
                        candidate,
                        &treatment_assignment,
                        prediction,
                        Some(strategy.as_str()),
                        effective_holdout,
                    )
                    .await?;
                if let Some(action_id) = persisted.action_id {
                    // Record the growth evidence row separately — it
                    // is measurement infrastructure, not causal
                    // bookkeeping. The prediction was already recorded
                    // atomically inside the transaction.
                    let _ = self
                        .repository
                        .record_dispatch_prediction(
                            self.workspace_id,
                            action_id,
                            prediction,
                            Some(strategy.as_str()),
                            effective_holdout,
                        )
                        .await;
                }
                dispatched_count += 1;
            }
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

/// Extracts the community unit_id from a community-engager decision_key.
///
/// The decision_key format is:
/// `decision:growth-intelligence:v{version}:community-engager:{target_id}:{cooldown_bucket}`
///
/// The unit_id is the subreddit derived from the target_id. Since the
/// target_id is a UUID and the subreddit is the human-readable identifier,
/// we use the target_id as the unit_id for experiment purposes. The
/// subreddit is recovered from the candidate's input_snapshot when needed
/// for measurement.
fn unit_id_from_decision_key(decision_key: &str) -> String {
    // Split by ':' and extract the target_id (5th segment, 0-indexed 4).
    let parts: Vec<&str> = decision_key.split(':').collect();
    match parts.get(4) {
        Some(s) => (*s).to_owned(),
        None => decision_key.to_owned(),
    }
}

/// Returns the cooldown window hours for a given template, used to compute
/// the logical_cycle_key. This must match the key_window_hours used in
/// the candidate's decision_key so the experiment identity aligns with
/// the idempotency identity.
fn key_window_for_template(policy: &GrowthIntelligencePolicy, template_id: &str) -> u32 {
    match template_id {
        "reddit-scanner" => policy.reddit_scanner_cooldown_hours,
        "press-pitch" => policy.press_pitch_cooldown_hours,
        "social-post" => policy.social_post_cooldown_hours,
        "community-engager" => policy.community_engager_cooldown_hours,
        "signal-inviter" => policy.signal_inviter_cooldown_hours,
        "growth-strategist" => policy.growth_strategist_cooldown_hours,
        _ => 24, // sensible default for unknown templates
    }
}
