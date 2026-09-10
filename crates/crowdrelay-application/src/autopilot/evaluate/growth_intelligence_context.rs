// GrowthIntelligence context arm — extracted from evaluate.rs to keep
// the orchestrator under the modularity contract line limit.
impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    async fn evaluate_growth_intelligence_context(
        &self,
        policy: &AutopilotPolicy,
        now: OffsetDateTime,
        _limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
    ) -> Result<(), AutopilotError> {
        let mut snapshots = self
            .repository
            .load_growth_intelligence_snapshots(self.workspace_id, now)
            .await?;
        // Report the North Star the world model actually resolved, so the cycle
        // record trends the metric the brain is optimizing rather than a second
        // figure derived somewhere else under the same name.
        report.north_star_observed = snapshots
            .first()
            .map(|snapshot| snapshot.world_model.north_star_current);
        // Walk-forward validation, then the hypothesis lifecycle transitions
        // it justifies. Extracted to `evaluate/hypothesis_validation.rs`: it
        // is one coherent job — judge each template against its own measured
        // outcomes — and it is the part of this cycle with no bearing on
        // candidate generation below.
        self.validate_hypotheses(&mut snapshots, now).await?;
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
        // Load the learned strategy posterior. This is the consumer the
        // posterior has been waiting for: the brain starts with the
        // operator's rules (from_world_model_with_hysteresis) and refines
        // them with observed evidence when there is enough confidence.
        let strategy_posterior = self
            .repository
            .load_brain_state(self.workspace_id, "strategy_posterior")
            .await?;
        // What the operator's rules alone would have chosen, kept beside what
        // the brain actually chose. Both go on every decision this cycle
        // writes: the pair is the difference between "the brain picked
        // community_first" and "the brain picked community_first because
        // measured outcomes overrode the rule that said otherwise", and only
        // the second is evidence that the loop closed.
        let mut strategy_prior: Option<GrowthStrategy> = None;
        let strategy = if let Some(first) = snapshots.first() {
            let hysteresis_strategy = GrowthStrategy::from_world_model_with_hysteresis(
                &first.world_model,
                previous_strategy,
            );
            strategy_prior = Some(hysteresis_strategy);
            // Refine the hysteresis strategy with the learned posterior.
            // The posterior overrides the prior only when it has ≥5
            // observations and the expected fan yield difference is large
            // enough (≥1 expected incremental fan). With no posterior or
            // insufficient evidence, the hysteresis strategy stands.
            match &strategy_posterior {
                Some((state, _)) => {
                    match serde_json::from_value::<
                        crowdrelay_brain::StateConditionedStrategyPosterior,
                    >(state.clone())
                    {
                        Ok(posterior) => {
                            let posterior_strategy = GrowthStrategy::from_world_model_with_posterior(
                                &first.world_model,
                                &posterior,
                            );
                            // The proof that learning changed behavior, not
                            // just belief. `hysteresis_strategy` is what the
                            // operator's rules alone would have picked this
                            // cycle; `posterior_strategy` is what the brain
                            // actually acts on. Silent when they agree —
                            // which is most cycles, and correctly
                            // unremarkable — so this only fires the moment
                            // learned evidence overrides the prior. Recorded
                            // on the cycle report (this crate carries no
                            // tracing dependency) so the worker's existing
                            // "autopilot cycle report" line surfaces it.
                            if posterior_strategy != hysteresis_strategy {
                                report.gi_dispatch_log.push(format!(
                                    "brain decision influenced by learning: strategy changed prior={} posterior={}",
                                    hysteresis_strategy.as_str(),
                                    posterior_strategy.as_str(),
                                ));
                            }
                            posterior_strategy
                        }
                        Err(_) => hysteresis_strategy,
                    }
                }
                None => hysteresis_strategy,
            }
        } else {
            GrowthStrategy::default()
        };
        // The unconsumed insights, deduplicated across snapshots.
        //
        // Every snapshot now carries the same workspace-wide set, so any
        // dispatch at all puts all of them in front of a worker and "consumed"
        // is a workspace-wide fact again. It was not while insights were
        // routed to their producing template: back then a dispatch of one
        // template retired every other template's insights unread, which is
        // why this used to be tracked per template.
        let mut pending_insights: std::collections::BTreeSet<Uuid> =
            std::collections::BTreeSet::new();
        // Collect all eligible candidates with their EFE scores
        // and strategy ranks, then sort by (strategy_rank,
        // efe_score) so the brain dispatches the best
        // opportunities first. When budget limits kick in,
        // the worst opportunities are the ones that get gated.
        let mut scored_candidates: Vec<ScoredCandidate> =
            Vec::with_capacity(snapshots.len());
        for snapshot in &snapshots {
            for insight in &snapshot.recent_insights {
                pending_insights.insert(insight.outcome_id);
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
        // ── Idle exploration: explore new horizons when all templates are on cooldown ──
        //
        // Kern's brain never idles — it always has event generators scanning for
        // new data. CrowdRelay's brain waits for cooldowns to expire, which means
        // it cycles every 5 minutes producing nothing when all templates are on
        // cooldown. This is the "idling" problem.
        //
        // When all templates returned no candidates (all on cooldown) AND the
        // brain's self-assessment says it needs to explore (Stagnant,
        // Regressing, or Initializing), dispatch a growth-strategist "explore
        // new horizons" run. This run asks the LLM to identify NEW platforms,
        // communities, and audiences the brain has not yet investigated —
        // Spotify playlists, Bandsintown, Facebook groups, Instagram, TikTok,
        // YouTube, podcast communities, local event listings.
        //
        // This has its own 24-hour cooldown (separate from the growth-
        // strategist's normal 12-hour cooldown) so it fires at most once per
        // day when the brain is idle. The idempotency key uses a 24-hour
        // window so the same dispatch does not recur within the window.
        if scored_candidates.is_empty()
            && let Some(candidate) =
                idle_exploration_candidate(&snapshots, policy, self.workspace_id, now)?
        {
            scored_candidates.push(candidate);
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
        #[derive(Clone, Debug)]
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
                Ok(d) => {
                    report.gi_dispatch_log.push(format!(
                        "experiment_design_loaded: template={} status={:?} holdout={} units={}",
                        template_id, d.experiment_status, d.holdout_probability, d.eligible_units.len()
                    ));
                    d
                }
                // Experiment design conflict — skip this group. The design
                // already exists from a prior cycle; the next cycle will
                // SELECT it successfully.
                //
                // Mark all candidates in this group as control so they're
                // excluded from the portfolio pool. Without this, the
                // candidate passes the control_indices filter, gets
                // selected by the portfolio, but falls through both
                // dispatch loops (non-experiment doesn't handle it,
                // treatment can't find it in arm_map) — the portfolio
                // reports "dispatched N" but nothing actually dispatches.
                Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
                    for (idx, _, _) in group_candidates {
                        control_indices.insert(*idx);
                    }
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
                    )
                    .with_assigned_at(now);
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
        //
        // Multiplied by agent execution health: an independent signal for
        // "is the worker layer producing usable outcomes right now",
        // distinct from the North Star trend metacognition tracks. A brain
        // that is Improving but whose only LLM provider is out of quota
        // should still size down — the two questions are orthogonal, and a
        // provider outage does not become growth-trend evidence for days.
        let sizing_multiplier = snapshots.first().map_or(1.0, |s| {
            s.metacognition.sizing_multiplier() * s.agent_execution_health.sizing_multiplier()
        });
        if let Some(first) = snapshots.first()
            && first.agent_execution_health.needs_attention()
        {
            report.gi_dispatch_log.push(format!(
                "agent execution health {}: dispatch budget scaled by {:.2}",
                first.agent_execution_health.as_str(),
                first.agent_execution_health.sizing_multiplier(),
            ));
        }
        let selection = portfolio::select_portfolio(
            &portfolio_candidates,
            &gi_policy,
            pending_measurement_count,
            self.workspace_id,
            &experimental_keys,
            sizing_multiplier,
        );
        let selected_keys = portfolio::selected_keys(&selection);
        report.gi_candidates = u32::try_from(scored_candidates.len()).unwrap_or(u32::MAX);
        report.gi_wait_reason = selection.wait_reason.clone();
        // The decision-time economic and epistemic record, per selected
        // candidate. `DecisionValue` is computed here and dropped, so without
        // this a later reader can only re-derive what the brain *would* decide
        // against posteriors that have since moved — a different question that
        // looks identical in a report. It rides in the decision's existing
        // `input_snapshot`, so there is no schema change.
        let decision_provenance =
            portfolio::decision_provenance(&selection, policy.version, &belief_origin);
        // What learning did to this decision, recorded on the decision itself.
        // Without it the strategy is visible and its provenance is not, so
        // "the brain changed its mind because of what it measured" could only
        // be inferred by re-deriving the rule-based strategy later — against a
        // world model that has since moved.
        let learning_provenance = learning_provenance(strategy_prior, strategy);
        // ── Dispatch phase ──
        // Which templates actually reached the agent service this cycle. Only
        // these carried their insights into a prompt, so only these may retire
        // them below.
        let mut dispatched_templates: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        // Dispatch non-experiment candidates (scanner, strategist).
        for i in &non_experiment_indices {
            let Some(scored) = scored_candidates.get(*i) else {
                continue;
            };
            let is_selected = !selection.do_nothing && selected_keys.contains(&scored.candidate.decision_key);
            report.gi_dispatch_log.push(format!(
                "non_experiment_check: idx={} key={} selected={} do_nothing={}",
                *i, scored.candidate.decision_key, is_selected, selection.do_nothing
            ));
            if !is_selected {
                continue;
            }
            // P1: persist candidate + prediction + initial evidence
            // atomically in one transaction. This guarantees the
            // prediction consistency invariant:
            // prediction_at_decision == prediction_persisted_in_initial_evidence.
            let mut candidate = scored.candidate.clone();
            attach_decision_provenance(
                &mut candidate,
                &decision_provenance,
                &learning_provenance,
            );
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
                if let AutopilotActionPayload::RequestAgentRun { template_id, .. } =
                    &scored.candidate.action
                {
                    dispatched_templates.insert(template_id.clone());
                }
                if persisted.decision_created {
                    report.decisions = report.decisions.saturating_add(1);
                }
                if persisted.action_created {
                    report.actions_enqueued = report.actions_enqueued.saturating_add(1);
                }
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
                report.gi_dispatch_log.push(format!(
                    "treatment_check: template={} key={} selected={} do_nothing={} selected_count={} arm={:?}",
                    template_id, candidate.decision_key, is_selected, selection.do_nothing, selection.selected.len(), arm
                ));
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
                        )
                        .with_assigned_at(now);
                    let mut candidate = candidate.clone();
                    attach_decision_provenance(
                &mut candidate,
                &decision_provenance,
                &learning_provenance,
            );
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
                        Ok(p) => {
                            report.gi_dispatch_log.push(format!(
                                "treatment_persist: template={} decision_created={} action_created={} throttled={} has_action_id={}",
                                template_id, p.decision_created, p.action_created, p.quota_throttled, p.action_id.is_some()
                            ));
                            p
                        }
                        // Conflict = action or assignment already exists. Skip
                        // this candidate and continue dispatching the rest.
                        Err(RepositoryError::Conflict | RepositoryError::ConflictBecause(_)) => {
                            report.gi_dispatch_log.push(format!(
                                "treatment_conflict: template={} key={}",
                                template_id, candidate.decision_key
                            ));
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
                    dispatched_templates.insert(template_id.clone());
                    if persisted.decision_created {
                        report.decisions = report.decisions.saturating_add(1);
                    }
                    if persisted.action_created {
                        report.actions_enqueued = report.actions_enqueued.saturating_add(1);
                    }
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
                        )
                        .with_assigned_at(now);
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
        // Only mark insights consumed once something actually dispatched.
        // Every dispatched prompt carries all of them, so one dispatch is
        // enough and which template it was does not matter. A cycle that
        // dispatched nothing put them in front of nobody, and retiring them
        // there would drop context nothing had read — so they stay unconsumed
        // and are re-evaluated next cycle.
        let consumed_ids: Vec<Uuid> = if dispatched_templates.is_empty() {
            Vec::new()
        } else {
            pending_insights.into_iter().collect()
        };
        if !consumed_ids.is_empty()
            && self
                .repository
                .mark_insights_consumed(self.workspace_id, &consumed_ids)
                .await
                .is_err()
        {
            // Not fatal — the dispatch idempotency key blocks a repeat inside
            // the cooldown window. But an insight that never gets marked is
            // re-evaluated every cycle forever, and the failure was silent, so
            // the only symptom was a brain that kept reconsidering the same
            // insight and nothing saying why.
            report.gi_dispatch_log.push(format!(
                "insight consumption failed for {} insight(s); they will be \
                 re-evaluated next cycle",
                consumed_ids.len(),
            ));
        }
        // Save the causal model checkpoint for fast startup
        // with delta replay on the next cycle. This is
        // best-effort — a failed checkpoint just means the
        // next cycle does a full replay.
        //
        // Best-effort, and no longer silent: a full replay reads every
        // resolved evidence row the workspace has ever produced, so a
        // checkpoint that has been failing for weeks is a cycle that has been
        // getting steadily more expensive with nothing reporting it.
        if self
            .repository
            .save_brain_state_checkpoint(self.workspace_id, &causal_model)
            .await
            .is_err()
        {
            report
                .gi_dispatch_log
                .push("causal model checkpoint failed; the next cycle replays all evidence".into());
        }
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
    learning: &serde_json::Value,
) {
    let identity = policy_content_identity(&candidate.policy_snapshot);
    let Some(object) = candidate.input_snapshot.as_object_mut() else {
        return;
    };
    // Attached to every candidate, including one the portfolio has no record
    // for. Which strategy the brain acted on, and whether learning chose it,
    // is true of the decision regardless of how the candidate reached it.
    object.insert("learning".to_owned(), learning.clone());
    let Some(record) = provenance.get(&candidate.decision_key) else {
        return;
    };
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

/// What learning did to this cycle's strategy choice.
///
/// `strategy_source` is the field that carries the claim: `posterior` means
/// measured outcomes overrode the rule-based strategy for this world state,
/// and `prior` means the rules and the evidence agreed — or that there was
/// not enough evidence to disagree. The two are deliberately not collapsed
/// into a boolean, because "no snapshots, so no strategy was derived at all"
/// is a third answer and reporting it as `prior` would be false.
fn learning_provenance(
    strategy_prior: Option<GrowthStrategy>,
    strategy_applied: GrowthStrategy,
) -> serde_json::Value {
    match strategy_prior {
        Some(prior) => serde_json::json!({
            "strategy_prior": prior.as_str(),
            "strategy_applied": strategy_applied.as_str(),
            "strategy_source": if prior == strategy_applied { "prior" } else { "posterior" },
        }),
        None => serde_json::json!({
            "strategy_applied": strategy_applied.as_str(),
            "strategy_source": "default",
        }),
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

/// The cooldown for the "explore new horizons" idle-exploration dispatch.
///
/// Separate from the growth-strategist's normal cooldown so the brain can
/// dispatch a normal strategist run (analyze existing data) and an idle
/// exploration run (find new platforms/communities) independently. The
/// idle exploration fires at most once per day when all templates are on
/// cooldown and the brain needs to explore.
const IDLE_EXPLORATION_COOLDOWN_HOURS: u32 = 24;

/// Builds an "explore new horizons" candidate when all templates are on
/// cooldown and the brain's self-assessment says it needs to explore.
///
/// This is the brain's proactive intelligence gathering — Kern's event
/// generators scan for new data; CrowdRelay's brain waits for cooldowns.
/// When every template is on cooldown, the brain would cycle producing
/// nothing. Instead, it dispatches a growth-strategist run that asks the
/// LLM to identify NEW platforms, communities, and audiences the brain
/// has not yet investigated — Spotify playlists, Bandsintown, Facebook
/// groups, Instagram, TikTok, YouTube, podcast communities, local event
/// listings.
///
/// The candidate carries `reason: "exploring new horizons — current
/// channels exhausted"` so the operator can see why the brain chose to
/// explore. The idempotency key uses a 24-hour window so the same
/// dispatch does not recur within the window.
fn idle_exploration_candidate(
    snapshots: &[crowdrelay_brain::GrowthIntelligenceSnapshot],
    policy: &AutopilotPolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Option<ScoredCandidate>, serde_json::Error> {
    use crowdrelay_brain::self_assessment::BrainState;

    // The brain's self-assessment is brain-wide (one state per tenant).
    // Take it from the first snapshot.
    let Some(first) = snapshots.first() else {
        return Ok(None);
    };
    let brain_state = first.metacognition.state;
    // Only explore when the brain needs to — Stagnant, Regressing, or
    // Initializing. Improving means the current channels are working.
    if !matches!(
        brain_state,
        BrainState::Stagnant | BrainState::Regressing | BrainState::Initializing
    ) {
        return Ok(None);
    }
    // Find the growth-strategist snapshot to check its cooldown.
    let strategist = snapshots
        .iter()
        .find(|s| s.template_id == "growth-strategist");
    let Some(strategist) = strategist else {
        return Ok(None);
    };
    // The idle exploration has its own cooldown, separate from the
    // growth-strategist's normal cooldown. Use `hours_since_last_run`
    // (any run, not just effective) so a failed run still counts.
    let hours_since = strategist.hours_since_last_run.unwrap_or(u32::MAX);
    if hours_since < IDLE_EXPLORATION_COOLDOWN_HOURS {
        return Ok(None);
    }
    // Build the "explore new horizons" prompt. The prompt asks the LLM
    // to identify NEW platforms and communities the brain has not yet
    // investigated. The brain validates the output against available
    // templates and does not blindly dispatch to unsupported platforms.
    let prompt = "The brain's current channels (Reddit, Telegram, Discord, Bandcamp, Metal Archives) are exhausted — fan growth is stagnant or regressing. Identify NEW platforms, communities, and audiences the band has not yet investigated. Consider: Spotify playlists, Bandsintown, Facebook groups, Instagram, TikTok, YouTube, podcast communities, local event listings, genre-specific forums. For each, report: platform name, audience size estimate, relevance to the band's genre, and how the brain could reach that audience. Prioritize platforms with the highest potential fan yield and lowest engagement friction. Write in Polish for the primary audience.";
    let prediction = DispatchPrediction {
        template_id: "growth-strategist".to_owned(),
        expected_new_fans: 0.0,
        expected_signal_installs: 0.0,
        context: crowdrelay_brain::DispatchContext::default(),
        target_key: None,
        creative_family: None,
    };
    let AutopilotPolicyConfig::GrowthIntelligence(ref domain_policy) = policy.config else {
        return Ok(None);
    };
    let action = AutopilotActionPayload::RequestAgentRun {
        template_id: "growth-strategist".to_owned(),
        prompt: prompt.to_owned(),
        priority: 5,
        tier: crowdrelay_brain::AgentTier::Basic,
    };
    let cooldown_bucket = (now.unix_timestamp() / 3600 / i64::from(IDLE_EXPLORATION_COOLDOWN_HOURS))
        * i64::from(IDLE_EXPLORATION_COOLDOWN_HOURS);
    Ok(Some(ScoredCandidate {
        candidate: DecisionCandidate {
            context: policy.context,
            subject: ActionSubject::Workspace(workspace_id),
            decision_kind: "request_agent_run",
            confidence: Confidence::MAX,
            disposition: disposition(
                policy.autonomy_level,
                Confidence::MAX,
                policy.minimum_confidence,
            ),
            reason: "exploring new horizons — current channels exhausted",
            input_snapshot: serde_json::json!({
                "brain_state": brain_state.as_str(),
                "idle_exploration": true,
                "hours_since_last_strategist_run": hours_since,
            }),
            policy_snapshot: policy_evidence(policy, domain_policy)?,
            action,
            decision_key: format!(
                "decision:growth-intelligence:v{v}:growth-strategist:idle-exploration:{cooldown_bucket}",
                v = policy.version
            ),
            action_idempotency_key: format!(
                "action:agent-run:growth-strategist:idle-exploration:{cooldown_bucket}"
            ),
        },
        prediction,
        efe_score: 0.0,
        strategy_rank: usize::MAX,
        treatment_stats: crowdrelay_brain::TreatmentAwareStats {
            expected_fans: 0.0,
            treatment_effect: 0.0,
            treatment_std: 1.0,
            predict_std: 1.0,
            confidence: 0,
            treatment_confidence: 0,
            use_treatment_effect: false,
            treatment_effect_y30: 0.0,
            treatment_std_y30: 1.0,
            treatment_confidence_y30: 0,
            uses_y30: false,
            p_meaningful_effect: 0.0,
            bridge_confidence: 0,
            bridge_is_reliable: false,
            evidence_quality: crowdrelay_brain::EvidenceQuality::Observational,
        },
        information_gain: 0.0,
        novelty: 1.0,
    }))
}
