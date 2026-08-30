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

        // ── Tenant operating preference ──
        //
        // Tenant preference MUST NOT filter, reorder, or otherwise
        // influence the economic candidate pipeline. The portfolio
        // optimizer ranks by DecisionValue.total() — preference only
        // affects cadence (cooldown multipliers in the snapshot
        // evaluation) and presentation (operator-facing surfacing).
        //
        // Hard invariant: a low-preference candidate with high
        // DecisionValue MUST remain economically selectable. Preference
        // reduces operator noise; it does not create an economic blind
        // spot.
        //
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
        // ── P0-1: Experiment population ≠ portfolio selection ──
        //
        // FULL SEPARATION: The experiment is created from the ELIGIBLE
        // population BEFORE portfolio selection, not from the SELECTED
        // candidates after. The experiment universe is ALL eligible
        // candidates. The portfolio decides how many treatment units
        // actually get dispatched, not which units exist in the experiment.
        //
        // Flow:
        //   candidates → group by intervention → create experiment (ALL eligible)
        //   → assign arms (treatment/control) → portfolio selects from
        //   TREATMENT-assigned candidates only → dispatch selected treatment
        //   → record non-selected treatment as withheld (action_id=None)
        //
        // This eliminates selection bias: the estimand is "effect among
        // all eligible candidates", not "effect among already-selected
        // winners."
        let holdout_probability = gi_policy.randomized_holdout_probability.clamp(0.0, 0.10);
        // Group ALL direct-action candidates by intervention (template_id).
        // Non-direct-action candidates (scanner, strategist) bypass
        // experiments and go directly to the portfolio.
        type ExperimentGroup = (String, Vec<(usize, DecisionCandidate, DispatchPrediction)>);
        let mut experiment_groups: Vec<ExperimentGroup> = Vec::new();
        let mut non_experiment_indices: Vec<usize> = Vec::new();
        for (i, (candidate, prediction, _efe, _rank, _stats)) in
            scored_candidates.iter().enumerate()
        {
            let template_id = match &candidate.action {
                AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
                    template_id.as_str()
                }
                _ => "",
            };
            let is_direct_action = !template_id.is_empty()
                && !matches!(template_id, "reddit-scanner" | "growth-strategist");
            if !is_direct_action {
                non_experiment_indices.push(i);
                continue;
            }
            if let Some(group) = experiment_groups.iter_mut().find(|(t, _)| t == template_id) {
                group.1.push((i, candidate.clone(), prediction.clone()));
            } else {
                experiment_groups.push((
                    template_id.to_owned(),
                    vec![(i, candidate.clone(), prediction.clone())],
                ));
            }
        }
        // For each intervention group, create the experiment design and
        // assign arms to ALL eligible units. Control units are removed
        // from the portfolio pool. Treatment units are marked
        // is_experimental for the portfolio optimizer.
        //
        // Maps decision_key → (arm, design, unit_id) for later use
        // during dispatch and withheld-treatment recording.
        #[derive(Clone)]
        enum ArmAssignment {
            Control,
            Treatment,
        }
        let mut arm_map: std::collections::HashMap<
            String,
            (ArmAssignment, crowdrelay_brain::ExperimentDesign, String, f64),
        > = std::collections::HashMap::new();
        // Track which scored_candidates indices are control (to be
        // removed from the portfolio pool).
        let mut control_indices: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        for (template_id, group_candidates) in &experiment_groups {
            let unit_kind = if template_id == "community-engager" {
                crowdrelay_brain::ExperimentUnitKind::TargetCommunity
            } else {
                crowdrelay_brain::ExperimentUnitKind::Workspace
            };
            let eligible_units: Vec<String> = if template_id == "community-engager" {
                group_candidates
                    .iter()
                    .map(|(_, c, _)| unit_id_from_decision_key(&c.decision_key))
                    .collect()
            } else {
                group_candidates
                    .iter()
                    .map(|(_, c, _)| c.decision_key.clone())
                    .collect()
            };
            let key_window_hours = key_window_for_template(&gi_policy, template_id);
            let logical_cycle_key = cooldown_window(now, key_window_hours).to_string();
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
            let is_insufficient_power =
                design.experiment_status == crowdrelay_brain::ExperimentStatus::InsufficientPower;
            if is_insufficient_power {
                design.holdout_probability = 0.0;
            }
            let effective_holdout = if is_insufficient_power {
                0.0
            } else {
                holdout_probability
            };
            for (idx, candidate, prediction) in group_candidates {
                let unit_id = if template_id == "community-engager" {
                    unit_id_from_decision_key(&candidate.decision_key)
                } else {
                    candidate.decision_key.clone()
                };
                let roll = deterministic_roll(&format!(
                    "{}:{}:{}:{}",
                    design.experiment_uuid, unit_id, design.assignment_round, template_id
                ));
                let is_control = effective_holdout > 0.0 && roll < effective_holdout;
                if is_control {
                    // Control arm: record assignment, no action dispatched.
                    let assignment = crowdrelay_brain::ExperimentAssignment::from_design(
                        &design,
                        &unit_id,
                        &unit_id,
                        crowdrelay_brain::TreatmentAssignment::Control,
                        prediction,
                        None,
                    );
                    self.repository
                        .record_experiment_assignment(
                            self.workspace_id,
                            &assignment,
                            Some(strategy.as_str()),
                        )
                        .await?;
                    control_indices.insert(*idx);
                    arm_map.insert(
                        candidate.decision_key.clone(),
                        (ArmAssignment::Control, design.clone(), unit_id, effective_holdout),
                    );
                } else {
                    // Treatment arm: mark for portfolio. The assignment
                    // will be persisted AFTER the portfolio selects
                    // this candidate for dispatch. If the portfolio
                    // does NOT select it, a withheld-treatment
                    // assignment is recorded with action_id=None.
                    arm_map.insert(
                        candidate.decision_key.clone(),
                        (ArmAssignment::Treatment, design.clone(), unit_id, effective_holdout),
                    );
                }
            }
        }
        // Build the portfolio pool: non-experiment candidates + treatment-
        // assigned candidates (control candidates are excluded).
        let portfolio_candidates: Vec<ScoredCandidate> = scored_candidates
            .iter()
            .enumerate()
            .filter(|(i, _)| !control_indices.contains(i))
            .map(|(_, c)| c.clone())
            .collect();
        // Build the set of treatment-assigned decision_keys for marking
        // candidates as is_experimental in the portfolio.
        let experimental_keys: std::collections::HashSet<String> = arm_map
            .iter()
            .filter(|(_, (arm, _, _, _))| matches!(arm, ArmAssignment::Treatment))
            .map(|(k, _)| k.clone())
            .collect();
        // Run the portfolio optimizer on the treatment + non-experiment
        // candidates only. Control candidates are NOT in the pool.
        let selection = portfolio::select_portfolio(
            &portfolio_candidates,
            &gi_policy,
            pending_measurement_count,
            self.workspace_id,
            &experimental_keys,
        );
        let selected_keys = portfolio::selected_keys(&selection);
        // ── Presentation metadata (post-selection) ──
        //
        // Computed AFTER portfolio selection. This is a presentation-
        // layer concept — it does NOT modify DecisionValue or any
        // economic value. A low-preference candidate that wins
        // economically is still dispatched; the metadata tells the
        // operator UI to de-emphasize it.
        //
        // All snapshots share the same tenant preference (per-workspace).
        let tenant_pref = snapshots
            .first()
            .map(|s| s.tenant_preference.clone())
            .unwrap_or_default();
        let pref_policy = match &policy.config {
            AutopilotPolicyConfig::GrowthIntelligence(gi) => gi.tenant_preference_policy.clone(),
            _ => TenantPreferencePolicy::default(),
        };
        // ── Dispatch phase ──
        let mut dispatched_count = 0usize;
        // Dispatch non-experiment candidates (scanner, strategist).
        for i in &non_experiment_indices {
            let Some((candidate, prediction, _efe, _rank, _stats)) = scored_candidates.get(*i)
            else {
                continue;
            };
            if selection.do_nothing || !selected_keys.contains(&candidate.decision_key) {
                continue;
            }
            // Inject presentation metadata into input_snapshot for audit.
            let template_id = match &candidate.action {
                AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
                    template_id.as_str()
                }
                _ => "",
            };
            let mut candidate = candidate.clone();
            if let Some(obj) = candidate.input_snapshot.as_object_mut() {
                let meta = tenant_pref.presentation_metadata(template_id, &pref_policy);
                obj.insert(
                    "presentation".to_owned(),
                    serde_json::to_value(&meta).unwrap_or_default(),
                );
            }
            let persisted = self.persist(&candidate, limits, report).await?;
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
        }
        // Dispatch treatment-assigned candidates that were selected by
        // the portfolio. Record withheld-treatment assignments for
        // treatment candidates NOT selected.
        for (template_id, group_candidates) in &experiment_groups {
            for (_idx, candidate, prediction) in group_candidates {
                let Some((arm, design, unit_id, effective_holdout)) =
                    arm_map.get(&candidate.decision_key)
                else {
                    continue;
                };
                match arm {
                    ArmAssignment::Control => {
                        // Already recorded above. Skip.
                        continue;
                    }
                    ArmAssignment::Treatment => {}
                }
                let is_selected =
                    !selection.do_nothing && selected_keys.contains(&candidate.decision_key);
                if is_selected {
                    // Treatment selected by portfolio → dispatch.
                    let treatment_assignment =
                        crowdrelay_brain::ExperimentAssignment::from_design(
                            design,
                            unit_id,
                            unit_id,
                            crowdrelay_brain::TreatmentAssignment::Treatment,
                            prediction,
                            None,
                        );
                    // Inject presentation metadata into input_snapshot for audit.
                    let mut candidate = candidate.clone();
                    if let Some(obj) = candidate.input_snapshot.as_object_mut() {
                        let meta = tenant_pref.presentation_metadata(template_id, &pref_policy);
                        obj.insert(
                            "presentation".to_owned(),
                            serde_json::to_value(&meta).unwrap_or_default(),
                        );
                    }
                    let persisted = self
                        .repository
                        .persist_treatment_with_assignment(
                            self.workspace_id,
                            &candidate,
                            &treatment_assignment,
                            prediction,
                            Some(strategy.as_str()),
                            *effective_holdout,
                        )
                        .await?;
                    if let Some(action_id) = persisted.action_id {
                        let _ = self
                            .repository
                            .record_dispatch_prediction(
                                self.workspace_id,
                                action_id,
                                prediction,
                                Some(strategy.as_str()),
                                *effective_holdout,
                            )
                            .await;
                        // P1-f: Emit Exposure provenance event for
                        // community-engager actions. The exposure is
                        // anonymous (fan_id=None) — we know the post was
                        // published but not who saw it. Attribution
                        // method is "action_completion" with confidence
                        // 1.0. This is the first link in the provenance
                        // chain: Exposure → Interaction → Conversion →
                        // Durability. The measurement system will emit
                        // temporal-association Conversion events later.
                        if template_id == "community-engager" {
                            let community = unit_id.clone();
                            let exposure = crowdrelay_brain::FanProvenanceEvent {
                                fan_id: None,
                                event_kind: crowdrelay_brain::ProvenanceEventKind::Exposure,
                                channel: "reddit".to_owned(),
                                source_target: Some(community.clone()),
                                community: Some(community),
                                campaign_id: None,
                                action_id: Some(action_id),
                                attribution_method: "action_completion".to_owned(),
                                attribution_confidence: 1.0,
                                occurred_at: now,
                            };
                            let _ = self
                                .repository
                                .record_fan_provenance_event(
                                    self.workspace_id,
                                    &exposure,
                                )
                                .await;
                        }
                    }
                    dispatched_count += 1;
                } else {
                    // Treatment NOT selected by portfolio → record
                    // withheld-treatment assignment with action_id=None.
                    // This unit was randomized to treatment but not
                    // dispatched due to budget constraints. It is
                    // distinct from Control (withheld by randomization)
                    // and from Treatment (dispatched). The measurement
                    // system measures its outcome like control, but the
                    // estimand interpretation differs.
                    let withheld_assignment =
                        crowdrelay_brain::ExperimentAssignment::from_design(
                            design,
                            unit_id,
                            unit_id,
                            crowdrelay_brain::TreatmentAssignment::Treatment,
                            prediction,
                            None, // action_id=None — not dispatched
                        );
                    let _ = self
                        .repository
                        .record_experiment_assignment(
                            self.workspace_id,
                            &withheld_assignment,
                            Some(strategy.as_str()),
                        )
                        .await;
                }
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
