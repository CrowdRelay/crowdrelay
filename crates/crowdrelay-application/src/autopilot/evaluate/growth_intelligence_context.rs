// GrowthIntelligence context arm — extracted from evaluate.rs to keep
// the orchestrator under the modularity contract line limit.
impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    async fn evaluate_growth_intelligence_context(
        &self,
        policy: &AutopilotPolicy,
        now: OffsetDateTime,
        _limits: &mut CycleLimits<'_>,
        _report: &mut AutopilotCycleReport,
    ) -> Result<(), AutopilotError> {
        let mut snapshots = self
            .repository
            .load_growth_intelligence_snapshots(self.workspace_id, now)
            .await?;
        // Report the North Star the world model actually resolved, so the cycle
        // record trends the metric the brain is optimizing rather than a second
        // figure derived somewhere else under the same name.
        _report.north_star_observed = snapshots
            .first()
            .map(|snapshot| snapshot.world_model.north_star_current);
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
        for snapshot in &mut snapshots {
            let template_evidence: Vec<_> = growth_evidence
                .iter()
                .filter(|e| {
                    e.opportunity_id
                        .as_ref()
                        .map(|id| id.starts_with(&snapshot.template_id))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            let result =
                crowdrelay_brain::validation::validate_evidence_for_promotion(&template_evidence);
            // Only adjust the hypothesis state if we have enough evidence
            // to validate meaningfully (OOS observations >= 5). Below
            // that, the default Active state is preserved.
            if result.out_of_sample.observations >= 5 && !result.passed {
                // Failed validation — degrade to Degraded (quarter budget)
                snapshot.hypothesis_state =
                    crowdrelay_brain::hypothesis::HypothesisState::Degraded;
            }
            // Passed validation (or insufficient evidence) — keep at Active
        }
        // Load the causal model from past predictions + outcomes.
        // The brain uses this to predict how many fans each
        // dispatch will produce, and learns from prediction errors.
        // The model and the identity of the beliefs behind it. A decision
        // persists the number the model gave it; without the identity it could
        // not say which beliefs produced that number, and the posteriors move
        // every cycle.
        let loaded_model = self.repository.load_causal_model(self.workspace_id).await?;
        let belief_origin = loaded_model.belief.clone();
        let causal_model = loaded_model.model;
        // The state-conditioned strategy posterior is NOT loaded here.
        //
        // It is still learned: the infra loader folds resolved growth evidence
        // into it during the causal model load and owns the write. It reaches
        // no decision, and this cycle used to load it, thread it through
        // candidate generation and discard it — which read as a belief
        // informing generation while nothing consulted it.
        //
        // Learning without a consumer is a defensible place to be. Plumbing
        // without a consumer is not: it is the part that makes a reader
        // conclude the belief is live. Wiring it up is a deliberate change to
        // these signatures — see `crowdrelay_brain::strategy_learning`.
        // Load the exploration memory from past dispatch
        // predictions. The brain uses this to compute novelty:
        // unexplored (template, context) pairs get an exploration
        // bonus in the EFE score.
        //
        // Propagated, not defaulted. An empty archive is a legitimate `Ok` —
        // a workspace that has explored nothing — so `unwrap_or_default()`
        // could only ever fire on a read failure, and it answered it by
        // claiming every (template, context) pair is unvisited. That is
        // maximum novelty on every candidate: the brain would re-explore
        // territory it already knows, and nothing would say why.
        let exploration_memory = self
            .repository
            .load_exploration_memory(self.workspace_id)
            .await?;
        // Derive the growth strategy from the world model, with
        // hysteresis — the brain doesn't flip-flop between
        // strategies every cycle when conditions are borderline.
        // The previous strategy is inferred from the most
        // recently dispatched template.
        //
        // Also propagated. `Ok(None)` already means "nothing has been
        // dispatched yet", so `unwrap_or(None)` only fired on a read failure —
        // and it answered it by reporting no previous strategy, which switches
        // hysteresis off. A transient database blip would produce exactly the
        // flip-flop the hysteresis exists to prevent, on a borderline world
        // model, once, with no trace.
        let last_template = self
            .repository
            .load_last_dispatched_template(self.workspace_id)
            .await?;
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
        //
        // Strategy rank comes first, then EFE — which is what the module's
        // own description says ("strategy -> template priority -> EFE
        // tie-break") and what nothing did. `strategy_rank` was computed by
        // both candidate producers, carried on every candidate, and read by
        // no one; the tuple it lived in hid that until it became a struct and
        // the compiler said the field was never read. So the strategy layer
        // has had no effect on ordering at all.
        //
        // It still cannot change what a candidate is worth: the portfolio
        // re-sorts by `DecisionValue.total()` and selects greedily from
        // there. This decides the order equals are offered in, which today —
        // with most candidates sitting on the same prior — is most of them.
        scored_candidates.sort_by(|a, b| {
            a.strategy_rank.cmp(&b.strategy_rank).then_with(|| {
                a.efe_score
                    .partial_cmp(&b.efe_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        // ── Tenant operating preference ──
        //
        // Tenant preference MUST NOT filter, reorder, or otherwise
        // influence the economic candidate pipeline. The portfolio
        // optimizer ranks by DecisionValue.total() — preference only
        // affects cadence (cooldown multipliers in the snapshot
        // evaluation). Presentation metadata is derived brain-side
        // but currently NOT persisted (TODO: wire to operator read
        // path when the UI supports it).
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
        // This one is economic, and it fails in the direction that acts.
        // `pending_measurement_count` is the whole of WAIT's
        // value-of-information: `n × avg_treatment_std × DECISION_SENSITIVITY`.
        // A read failure defaulted to zero, WAIT lost its entire epistemic
        // value, and the brain became more willing to dispatch — a database
        // error making an autonomous system more active, silently. Zero
        // outstanding measurements is a real state the query returns as `Ok(0)`;
        // not knowing is not that state.
        let pending_measurement_count = self
            .repository
            .count_pending_measurements(self.workspace_id)
            .await?;
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
        for (i, scored) in
            scored_candidates.iter().enumerate()
        {
            let template_id = match &scored.candidate.action {
                AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
                    template_id.as_str()
                }
                _ => "",
            };
            let is_direct_action = !template_id.is_empty()
                && !matches!(
                    template_id,
                    "reddit-scanner" | "telegram-scanner" | "metal-archives-scanner" | "bandcamp-scanner" | "growth-strategist"
                );
            if !is_direct_action {
                non_experiment_indices.push(i);
                continue;
            }
            if let Some(group) = experiment_groups.iter_mut().find(|(t, _)| t == template_id) {
                group
                    .1
                    .push((i, scored.candidate.clone(), scored.prediction.clone()));
            } else {
                experiment_groups.push((
                    template_id.to_owned(),
                    vec![(i, scored.candidate.clone(), scored.prediction.clone())],
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
            let (unit_kind, eligible_units) = if template_id == "community-engager" {
                (
                    crowdrelay_brain::ExperimentUnitKind::TargetCommunity,
                    group_candidates
                        .iter()
                        .map(|(_, c, _)| unit_id_from_decision_key(&c.decision_key))
                        .collect::<Vec<_>>(),
                )
            } else {
                // Direct-action templates (social-post, telegram-poster,
                // discord-poster, press-pitch, signal-inviter) use Campaign
                // as the unit kind. Each dispatch is a separate campaign —
                // the unit_id is the action_idempotency_key, which is unique
                // per dispatch because it includes the cooldown bucket.
                //
                // The experiment window spans 30 days (720 hours) instead of
                // the cooldown window, so multiple dispatches within that
                // window pool into one experiment. With a 2-7 day cooldown,
                // that yields 4-15 campaigns per experiment — enough for a
                // control arm at 10% holdout.
                (
                    crowdrelay_brain::ExperimentUnitKind::Campaign,
                    group_candidates
                        .iter()
                        .map(|(_, c, _)| c.action_idempotency_key.clone())
                        .collect::<Vec<_>>(),
                )
            };
            let experiment_window_hours = if template_id == "community-engager" {
                key_window_for_template(&gi_policy, template_id)
            } else {
                EXPERIMENT_WINDOW_HOURS
            };
            let logical_cycle_key = cooldown_window(now, experiment_window_hours).to_string();
            let mut design = match self
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
                .await
            {
                Ok(d) => d,
                // Experiment design conflict — skip this group. The design
                // already exists from a prior cycle; the next cycle will
                // SELECT it successfully.
                Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
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
                    // Campaign unit: each dispatch is a separate campaign,
                    // identified by its action_idempotency_key (unique per
                    // dispatch because it includes the cooldown bucket).
                    candidate.action_idempotency_key.clone()
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
                    match self
                        .repository
                        .record_experiment_assignment(
                            self.workspace_id,
                            &assignment,
                            Some(strategy.as_str()),
                        )
                        .await
                    {
                        Ok(()) => {}
                        // Assignment already exists — skip without aborting.
                        Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
                            control_indices.insert(*idx);
                            arm_map.insert(
                                candidate.decision_key.clone(),
                                (ArmAssignment::Control, design.clone(), unit_id, effective_holdout),
                            );
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }
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
        //
        // Metacognition sizing_multiplier: the brain's self-assessment
        // scales the dispatch budget. When Initializing or Regressing,
        // the brain acts cautiously (fewer dispatches). When Improving,
        // full budget. Taken from the first snapshot because metacognition
        // is brain-wide (one state per tenant), not per-template.
        let sizing_multiplier = snapshots
            .first()
            .map(|s| s.metacognition.sizing_multiplier())
            .unwrap_or(1.0);
        let selection = portfolio::select_portfolio(
            &portfolio_candidates,
            &gi_policy,
            pending_measurement_count,
            self.workspace_id,
            &experimental_keys,
            sizing_multiplier,
        );
        let selected_keys = portfolio::selected_keys(&selection);
        // The decision-time economic and epistemic record, per selected
        // candidate. `DecisionValue` is computed here and dropped, so without
        // this a later reader can only re-derive what the brain *would* decide
        // against posteriors that have since moved — a different question that
        // looks identical in a report. It rides in the decision's existing
        // `input_snapshot`, so there is no schema change.
        let decision_provenance =
            portfolio::decision_provenance(&selection, policy.version, &belief_origin);
        // ── Dispatch phase ──
        let mut dispatched_count = 0usize;
        // Dispatch non-experiment candidates (scanner, strategist).
        for i in &non_experiment_indices {
            let Some(scored) = scored_candidates.get(*i) else {
                continue;
            };
            if selection.do_nothing || !selected_keys.contains(&scored.candidate.decision_key) {
                continue;
            }
            // P1: persist candidate + prediction + initial evidence
            // atomically in one transaction. This guarantees the
            // prediction consistency invariant:
            // prediction_at_decision == prediction_persisted_in_initial_evidence.
            let mut candidate = scored.candidate.clone();
            attach_decision_provenance(&mut candidate, &decision_provenance);
            let persisted = match self
                .repository
                .persist_candidate_with_evidence(
                    self.workspace_id,
                    &candidate,
                    &scored.prediction,
                    Some(strategy.as_str()),
                    0.0,
                    &TraceContext::root(self.workspace_id),
                )
                .await
            {
                Ok(p) => p,
                // Conflict = candidate already exists or is in-flight. Skip
                // this candidate and continue dispatching the rest.
                Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if persisted.action_id.is_some() {
                dispatched_count += 1;
            }
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
                    let mut candidate = candidate.clone();
                    attach_decision_provenance(&mut candidate, &decision_provenance);
                    let persisted = match self
                        .repository
                        .persist_treatment_with_assignment(
                            self.workspace_id,
                            &candidate,
                            &treatment_assignment,
                            prediction,
                            Some(strategy.as_str()),
                            *effective_holdout,
                            &TraceContext::root(self.workspace_id),
                        )
                        .await
                    {
                        Ok(p) => p,
                        // Conflict = action or assignment already exists. Skip
                        // this candidate and continue dispatching the rest.
                        Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    if let Some(action_id) = persisted.action_id {
                        // P1: The prediction and initial evidence are now
                        // recorded atomically inside
                        // persist_treatment_with_assignment — no
                        // post-commit record_dispatch_prediction needed.
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
                    match self
                        .repository
                        .record_experiment_assignment(
                            self.workspace_id,
                            &withheld_assignment,
                            Some(strategy.as_str()),
                        )
                        .await
                    {
                        Ok(()) => {}
                        // Assignment already exists — skip without aborting.
                        Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
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
        // The strategy posterior is deliberately NOT saved here. This cycle
        // holds it by shared reference and never mutates it, so the write was
        // a copy of what the load returned — and the load ends in
        // `unwrap_or_default()`, so a deserialization failure or a read error
        // turned into an empty posterior that was then written back over
        // everything the learner had accumulated. One clobber, all history
        // gone, no error anywhere.
        //
        // `apply_evidence_to_stored_strategy_posterior` in the infra loader is
        // the single writer of this key. It runs earlier in this same cycle,
        // as part of the causal model load, and it refuses to write at all
        // when it cannot read what is already there.
        Ok(())
    }
}

/// The experiment window for direct-action templates (social-post,
/// telegram-poster, discord-poster, press-pitch, signal-inviter).
///
/// Each dispatch is a separate campaign unit. The experiment window spans
/// 30 days so that multiple dispatches (governed by 2-7 day cooldowns) pool
/// into one experiment design. With a 2-day cooldown, that yields ~15
/// campaigns per experiment — enough for a control arm at 10% holdout.
///
/// Community-engager uses its own cooldown window as the experiment window
/// because each target community is itself a unit, so one cycle already
/// contains multiple units.
const EXPERIMENT_WINDOW_HOURS: u32 = 24 * 30;

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
/// The cooldown window a template's idempotency and logical-cycle keys are
/// bucketed by.
///
/// Exhaustive on [`WorkerTemplate`] rather than a string match with a
/// default. The default was 24 hours, and `discord-poster` fell into it while
/// its policy cooldown was 48 — so its key would have rotated twice inside
/// one cooldown, letting the same dispatch be raised again while it was still
/// meant to be resting. A missing arm is now a compile error instead of a
/// number that happens to be wrong.
fn key_window_for_template(policy: &GrowthIntelligencePolicy, template_id: &str) -> u32 {
    let Some(template) = WorkerTemplate::parse(template_id) else {
        // Not a growth-intelligence template. A day is a safe bucket for a
        // question this function was never meant to answer.
        return 24;
    };
    match template {
        WorkerTemplate::RedditScanner => policy.reddit_scanner_cooldown_hours,
        WorkerTemplate::TelegramScanner => policy.telegram_scanner_cooldown_hours,
        WorkerTemplate::MetalArchivesScanner => policy.metal_archives_scanner_cooldown_hours,
        WorkerTemplate::BandcampScanner => policy.bandcamp_scanner_cooldown_hours,
        WorkerTemplate::PressPitch => policy.press_pitch_cooldown_hours,
        WorkerTemplate::SocialPost => policy.social_post_cooldown_hours,
        WorkerTemplate::TelegramPoster => policy.telegram_poster_cooldown_hours,
        WorkerTemplate::DiscordPoster => policy.discord_poster_cooldown_hours,
        WorkerTemplate::CommunityEngager => policy.community_engager_cooldown_hours,
        WorkerTemplate::SignalInviter => policy.signal_inviter_cooldown_hours,
        WorkerTemplate::GrowthStrategist => policy.growth_strategist_cooldown_hours,
    }
}

/// Merges the decision-time record into a candidate's `input_snapshot`.
///
/// Additive under a single key. The snapshot is stored raw and returned raw by
/// the decision-evidence endpoint, so a candidate that has no record — one
/// dispatched outside the portfolio, or from a build before this existed —
/// simply lacks the key rather than carrying an empty or invented one.
fn attach_decision_provenance(
    candidate: &mut DecisionCandidate,
    provenance: &std::collections::HashMap<String, serde_json::Value>,
) {
    let Some(record) = provenance.get(&candidate.decision_key) else {
        return;
    };
    let identity = policy_content_identity(&candidate.policy_snapshot);
    if let Some(object) = candidate.input_snapshot.as_object_mut() {
        let mut record = record.clone();
        if let Some(policy) = record
            .get_mut("policy")
            .and_then(serde_json::Value::as_object_mut)
        {
            policy.insert(
                "policy_identity".to_owned(),
                serde_json::Value::String(identity),
            );
        }
        object.insert("decision_value".to_owned(), record);
    }
}

/// A deterministic content identity for the policy that constrained a decision.
///
/// `policy_version` is a counter on a mutable row. The row is updated in place
/// and there is no history table, so the version a decision recorded resolves
/// to whatever that row holds today — which is why the full `policy_snapshot`
/// is captured beside it and remains the authoritative record.
///
/// What the snapshot could not do is answer "is this the same policy as that
/// one" without a field-by-field comparison, or survive being summarised. The
/// hash gives grouping and equality: identical semantic policy hashes
/// identically, materially different policy does not, and neither answer
/// changes when the row is later edited.
///
/// `serde_json` orders object keys, so serializing the snapshot is already
/// canonical for a given value — no separate canonicaliser, and no new table.
pub(crate) fn policy_content_identity(policy_snapshot: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};

    let canonical = serde_json::to_string(policy_snapshot).unwrap_or_default();
    let digest = Sha256::digest(canonical.as_bytes());
    // Half the digest. Enough to identify a policy among a workspace's
    // handful, short enough to read in a decision record.
    let mut identity = String::from("sha256:");
    for byte in digest.iter().take(16) {
        identity.push_str(&format!("{byte:02x}"));
    }
    identity
}
