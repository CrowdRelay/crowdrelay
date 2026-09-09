//! Replaying resolved growth evidence into the brain's beliefs.
//!
//! Split out of the loader so it stays inside the source-size ratchet, and
//! because it is one coherent job: take rows whose outcomes are known and fold
//! them into the causal model, the calibration trackers and the strategy
//! posterior. Nothing here talks to the world model or to candidate
//! generation.
//!
//! The distinction this module exists to keep straight is between an
//! observation and a counterfactual. Every row in the learning batch updates
//! the outcome model. Only control rows form the contrast the treated rows are
//! measured against, and that contrast is earned per horizon and may come from
//! an earlier batch than the row it serves.

use super::super::super::*;

/// Applies a batch of growth evidence to the causal model, updating the
/// outcome model, context effects, regime-isolated calibration, the
/// Y14/Y30 treatment-effect posteriors, and the Y14→Y30 bridge.
///
/// CALIBRATION REGIME ISOLATION: Y14 treatment-effect observations are
/// recorded to the Y14Bridged regime tracker, Y30 treatment-effect
/// observations to the Y30Direct regime tracker, and outcome model
/// observations to the OutcomeModel regime tracker. This ensures that
/// a badly calibrated observational predictor cannot distort uncertainty
/// for the randomized treatment estimator.
/// The control arm's average outcome, per experiment.
///
/// `None` for a horizon means no control unit has a resolved outcome on it
/// yet, which is the difference between having a comparison and only claiming
/// one. The caller uses that distinction to decide whether a treated row can
/// be called randomised evidence at all.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ControlMean {
    y14: Option<f64>,
    y30: Option<f64>,
}

/// Averages the control arm per experiment so treated rows can be measured
/// against it.
pub(super) fn control_arm_means<'a>(
    evidence: impl IntoIterator<Item = &'a crowdrelay_brain::GrowthEvidence>,
) -> std::collections::HashMap<uuid::Uuid, ControlMean> {
    let mut sums: std::collections::HashMap<uuid::Uuid, (f64, u32, f64, u32)> =
        std::collections::HashMap::new();
    for ev in evidence {
        if ev.treatment.is_treatment() {
            continue;
        }
        let Some(experiment_uuid) = ev.experiment_uuid else {
            // A control row that cannot say which experiment it belongs to
            // cannot be anyone's counterfactual. Counting it against every
            // experiment would be worse than counting it against none.
            continue;
        };
        let entry = sums.entry(experiment_uuid).or_insert((0.0, 0, 0.0, 0));
        if let Some(y14) = ev.observed_incremental_fans {
            entry.0 += y14;
            entry.1 += 1;
        }
        if let Some(y30) = ev.y30_outcome() {
            entry.2 += y30;
            entry.3 += 1;
        }
    }
    sums.into_iter()
        .map(|(experiment_uuid, (y14_sum, y14_n, y30_sum, y30_n))| {
            (
                experiment_uuid,
                ControlMean {
                    y14: (y14_n > 0).then(|| y14_sum / f64::from(y14_n)),
                    y30: (y30_n > 0).then(|| y30_sum / f64::from(y30_n)),
                },
            )
        })
        .collect()
}

/// Replays evidence into the causal model, contrasting treated rows against
/// the control arm found in the same batch.
///
/// The single-slice form is right whenever the caller holds every row the
/// batch could need — a full replay, or a test. Delta replay does not: see
/// [`apply_evidence_to_model_with_contrast`].
pub(super) fn apply_evidence_to_model(
    model: &mut crowdrelay_brain::CausalModel,
    evidence: &[crowdrelay_brain::GrowthEvidence],
) {
    apply_evidence_to_model_with_contrast(model, evidence, &[], None);
}

/// Replays `evidence`, contrasting it against the control arm in `evidence`
/// **plus** `extra_contrast`.
///
/// The two slices are not interchangeable. Everything in `evidence` is learned
/// from: the outcome model updates from every row, control ones included.
/// `extra_contrast` is only ever read for its control means — those rows were
/// learned from in an earlier batch and replaying them would count them twice.
///
/// `checkpoint` is the delta cursor timestamp. When `Some`, only horizons
/// newer than the checkpoint are replayed — the 3d/14d/30d per-horizon
/// cursors prevent double-counting. When `None` (full replay), all
/// available outcomes are learned from.
///
/// This exists because the learning cursor and the randomisation do not agree
/// about batches. The cursor is `resolved_at`, and an experiment's treated
/// units resolve when their own measurements finish, which is not the same day
/// for all of them. The control arm resolves once, alongside the first of them.
/// Every treated row after that arrived in a batch with no control arm in it,
/// was correctly capped at quasi-experimental, and contributed a raw pre/post
/// difference — a randomised experiment silently degrading to an observational
/// one because of when a checkpoint happened to be taken.
pub(super) fn apply_evidence_to_model_with_contrast(
    model: &mut crowdrelay_brain::CausalModel,
    evidence: &[crowdrelay_brain::GrowthEvidence],
    extra_contrast: &[crowdrelay_brain::GrowthEvidence],
    checkpoint: Option<OffsetDateTime>,
) {
    use crowdrelay_brain::{
        CausalEstimand, DispatchPrediction, EstimationRegime, ExecutionStatus, PredictionOutcome,
    };

    // The active causal estimand. This is the explicit domain decision
    // that determines which evidence rows contribute to the treatment-
    // effect posterior. SQL provides eligible observations; the causal
    // layer chooses the estimand. Do NOT let naming outrun identification.
    //
    // ITT is the safest default: it includes all assigned units, uses the
    // arm (Z) as the treatment indicator, and does not exclude based on
    // execution_status. This avoids the semantic shortcut of equating
    // `execution_status = executed` with "TOT is identified".
    let estimand = CausalEstimand::IntentToTreat;

    // Counters for the replay summary — make a no-op replay visible.
    let mut outcome_updates = 0u32;
    let mut y14_treatment_updates = 0u32;
    let mut y30_treatment_updates = 0u32;
    let mut bridge_updates = 0u32;

    // Per-horizon gating: in delta replay, only update the posteriors for
    // horizons that are new since the checkpoint. This prevents
    // double-counting — the 30d measurement stamps its own cursor without
    // changing observed_fans, so the outcome model must not be updated
    // again for it.
    //
    // A horizon is "new" when its replay timestamp is newer than the
    // checkpoint, or when there is no checkpoint (full replay). A NULL
    // per-horizon timestamp falls back to `resolved_at`: a fully resolved
    // row whose per-horizon columns were never stamped (legacy rows, or
    // evidence inserted directly by tests) is still new if it resolved
    // after the checkpoint. Only when both the per-horizon timestamp AND
    // `resolved_at` are NULL is the horizon genuinely incomplete.
    let horizon_is_new = |ts: Option<OffsetDateTime>, fallback: Option<OffsetDateTime>| -> bool {
        match (checkpoint, ts.or(fallback)) {
            (None, _) => true,          // full replay — learn from everything
            (Some(_cp), None) => false, // measurement not completed yet
            (Some(cp), Some(ts)) => ts > cp,
        }
    };

    // Intent-to-treat compares the arms. The control rows in this batch are
    // that comparison, so they are gathered first and the treated rows are
    // measured against them.
    //
    // Feeding a control row to `update_treatment_effect_for_target` — which is
    // what happened before — hands the posterior a control unit's own pre/post
    // drift as though it were a treatment effect, and leaves the treated rows
    // contributing their own pre/post difference with nothing subtracted. The
    // result is a number built entirely from treated units, labelled ITT. A
    // control arm that does not enter the arithmetic is not a control arm.
    //
    // Gathered over both slices, because a control arm that resolved in an
    // earlier batch is still this treated row's counterfactual. `evidence`
    // first, so a row present in both is not averaged in twice.
    let control_means =
        control_arm_means(evidence.iter().chain(extra_contrast.iter().filter(|extra| {
            // A row can legitimately arrive in both slices — the batch that
            // resolves the control arm also carries it. Averaging it in twice
            // would not change the mean, but it would change the count, so
            // keep the identity check rather than relying on that.
            //
            // When `experiment_assignment_id` is `None` (legacy rows), the
            // identity check falls back to `action_id` to avoid double-counting
            // the same row. Two rows with `None` assignment IDs and the same
            // action_id are the same row.
            if extra.experiment_assignment_id.is_some() {
                !evidence
                    .iter()
                    .any(|seen| seen.experiment_assignment_id == extra.experiment_assignment_id)
            } else {
                !evidence.iter().any(|seen| {
                    seen.experiment_assignment_id.is_none() && seen.action_id == extra.action_id
                })
            }
        })));

    for ev in evidence {
        let template = extract_template_from_opportunity(&ev.opportunity_id);
        let subreddit_type = ev.context.subreddit_type.as_deref();
        // The per-target level of the hierarchy is rebuilt from here. Rows
        // written before the column existed carry `None` and teach the
        // template and audience type only, which is exactly what they used
        // to teach — replaying old evidence cannot invent target knowledge
        // it never recorded.
        let target_key = ev.target_key.as_deref();
        // Scale the observation variance by the evidence quality multiplier.
        // Higher quality evidence (randomized holdout) gets a lower variance
        // → moves the posterior more. Lower quality evidence (observational)
        // gets a higher variance → barely moves the posterior. This prevents
        // weak pre/post evidence from dominating strong causal evidence.
        // The quality a row has earned, not the one it was labelled with at
        // dispatch. `evidence_quality` records that the unit was randomised;
        // whether that randomisation survived the measurement window is a
        // separate fact, and `final_contamination` is where it lives.
        let mut evidence_quality = ev.effective_evidence_quality();

        // ── Partial resolution downweighting ──
        //
        // A partially resolved row (partial_resolution_count > 0,
        // resolved_at IS NULL) carries an intermediate checkpoint
        // observation — typically a 7-day or 14-day measurement that
        // landed before the full 30-day outcome. The causal model can
        // learn from it, but with downweighted quality: the long-
        // horizon outcome may contradict the short-horizon signal, so
        // the intermediate observation is treated as Observational
        // regardless of the experiment design.
        //
        // This mirrors Kern's multi-checkpoint settling: intermediate
        // checkpoints (1-day, 7-day) update the posterior with higher
        // observation variance, while the final checkpoint gets full
        // weight. Here, partial resolution → Observational (weight
        // 0.5), full resolution → earned quality (weight up to 1.0).
        let is_partial = ev.partial_resolution_count > 0 && ev.resolved_at.is_none();
        if is_partial {
            evidence_quality = evidence_quality.min_observational();
        }

        // Update the outcome model (P(Y|action,context)) from the raw
        // observed fan count — NOT the DiD estimate. The outcome model
        // learns the expected raw fan count given an action and context.
        // The treatment-effect posterior (updated below) learns from the
        // counterfactual-adjusted DiD estimate. These are separate learning
        // targets and must not be conflated.
        //
        // The outcome model is ALWAYS updated from all rows regardless of
        // estimand — it learns the raw expected fan count, which is
        // estimand-agnostic. The estimand only gates the treatment-effect
        // posterior.
        //
        // We use `observed_fans` (raw count) when available. If only the
        // incremental estimate is available (legacy evidence rows), we
        // skip the outcome model update rather than feeding it a DiD
        // estimate that would be clamped to 0 on negative values.
        //
        // Per-horizon gating: the outcome model updates when the 14d
        // horizon is new (the 14d value is the final `observed_fans`),
        // or when the 3d horizon is new and the 14d hasn't landed yet
        // (the 3d value is the only observation so far). The 30d
        // measurement does NOT change `observed_fans`, so it must not
        // trigger an outcome model update.
        let outcome_is_new = horizon_is_new(ev.replayed_14d_at, ev.resolved_at)
            || (horizon_is_new(ev.replayed_3d_at, ev.resolved_at) && ev.replayed_14d_at.is_none());
        if outcome_is_new && let Some(raw_fans) = ev.observed_fans {
            let prediction = DispatchPrediction {
                template_id: template.clone(),
                expected_new_fans: ev.predicted_fans,
                expected_signal_installs: ev.predicted_signal_installs,
                context: ev.context.clone(),
                target_key: ev.target_key.clone(),
                creative_family: ev.creative_family,
            };
            let outcome = PredictionOutcome::from_observation(prediction, raw_fans, 0.0);
            model.update(&outcome);
            outcome_updates += 1;
        }

        // Determine whether this evidence row contributes to the
        // treatment-effect posterior under the active estimand.
        //
        // The estimand's `includes_in_treatment_effect` method is the
        // ONLY place that decides this — not SQL, not ad-hoc filtering.
        // If execution_status is missing (legacy rows), default to
        // Executed for treatment arm and Control for control arm, which
        // preserves the old behavior under ITT.
        let execution_status = ev
            .execution_status
            .unwrap_or(if ev.treatment.is_treatment() {
                ExecutionStatus::Executed
            } else {
                ExecutionStatus::Control
            });
        let contributes_to_tau =
            estimand.includes_in_treatment_effect(ev.treatment.is_treatment(), execution_status);

        if !contributes_to_tau {
            continue;
        }
        // Control rows are the counterfactual, not an effect. They have already
        // been folded into `control_means`; passing them on would teach the
        // treatment-effect posterior that withholding an action is an action.
        if !ev.treatment.is_treatment() {
            continue;
        }
        // The contrast this treated row is measured against. Without a
        // resolved control arm there is no randomised comparison to make, so
        // the row falls back to its own difference-in-differences and is capped
        // at the quasi-experiment it then is.
        //
        // The contrast is earned *per horizon*, not per experiment.
        // [`ControlMean`] carries `Option` per horizon precisely because a
        // control arm can be resolved on Y14 and still pending on Y30 — the
        // horizons close 16 days apart, so during that window every experiment
        // is in exactly that state. Reading only `contrast.is_some()` gave a
        // treated row full `RandomizedHoldout` weight on a horizon whose
        // control mean did not exist, and then quietly subtracted `0.0` for it:
        // a raw pre/post difference, labelled and weighted as a randomised
        // contrast. That is the promotion of unknown evidence to clean
        // evidence, and it biases τ away from zero by the control arm's own
        // secular drift.
        let contrast = ev.experiment_uuid.and_then(|id| control_means.get(&id));
        let y14_contrast = contrast.and_then(|mean| mean.y14);
        let y30_contrast = contrast.and_then(|mean| mean.y30);
        let earned_quality_for = |horizon_contrast: Option<f64>| {
            if horizon_contrast.is_some() {
                evidence_quality
            } else {
                evidence_quality.min_quasi_experimental()
            }
        };

        // Update the Y14 treatment-effect posterior from the incremental
        // outcome. The `observed_incremental_fans` field is the
        // counterfactual-adjusted τ estimate (already IPW-corrected if
        // propensity is available). The observation variance is scaled by
        // the evidence quality — weak evidence barely moves the posterior.
        //
        // Y14 treatment-effect calibration is recorded to the Y14Bridged
        // regime tracker — separate from Y30Direct and OutcomeModel.
        //
        // Per-horizon gating: the Y14 posterior updates only when the 14d
        // horizon is new. The 3d measurement does not produce an
        // incremental estimate, and the 30d measurement updates the Y30
        // posterior, not Y14.
        if horizon_is_new(ev.replayed_14d_at, ev.resolved_at)
            && let Some(outcome_y14) = ev.observed_incremental_fans
        {
            let earned_quality = earned_quality_for(y14_contrast);
            let tau_y14 = outcome_y14 - y14_contrast.unwrap_or(0.0);
            let obs_var = 2.0 * tau_y14.abs().max(1.0) * earned_quality.variance_multiplier();
            model.update_treatment_effect_for_target(
                &template,
                subreddit_type,
                target_key,
                tau_y14,
                obs_var,
                earned_quality,
            );
            y14_treatment_updates += 1;
            // Record Y14Bridged calibration with the actual measurement-
            // determined evidence quality, not a synthesized one.
            model.calibration.record_by_regime(
                EstimationRegime::Y14Bridged,
                &template,
                ev.predicted_fans,
                2.0,
                tau_y14,
                subreddit_type,
                None,
                earned_quality.as_str(),
            );
        }

        // When Y30 (durable) is available, update the Y30 treatment-effect
        // posterior, the Y30Direct calibration tracker, and the Y14→Y30
        // bridge.
        //
        // Per-horizon gating: the Y30 posterior updates only when the 30d
        // horizon is new. The 3d and 14d measurements do not produce a
        // durable-fans observation.
        if horizon_is_new(ev.replayed_30d_at, ev.resolved_at)
            && let Some(outcome_y30) = ev.y30_outcome()
        {
            // Y30 treatment-effect update (North Star). Scaled by evidence
            // quality — same rationale as Y14, and earned against the Y30
            // control mean specifically. Y30 is the horizon that stays pending
            // longest, so it is the one most often claimed without a contrast.
            let earned_quality = earned_quality_for(y30_contrast);
            let y30_fans = outcome_y30 - y30_contrast.unwrap_or(0.0);
            let obs_var = 2.0 * y30_fans.abs().max(1.0) * earned_quality.variance_multiplier();
            model.update_treatment_effect_y30_for_target(
                &template,
                subreddit_type,
                target_key,
                y30_fans,
                obs_var,
                earned_quality,
            );
            y30_treatment_updates += 1;
            // Y30Direct calibration — isolated from Y14Bridged and
            // OutcomeModel. A bad OutcomeModel calibration cannot distort
            // Y30Direct uncertainty. Uses the actual measurement-determined
            // evidence quality.
            model.calibration.record_by_regime(
                EstimationRegime::Y30Direct,
                &template,
                ev.predicted_fans,
                2.0,
                y30_fans,
                subreddit_type,
                None,
                earned_quality.as_str(),
            );
            // Y14→Y30 bridge: update when both outcomes are available.
            //
            // Both sides must carry the same contrast. `y30_fans` has the
            // control arm's mean subtracted, so feeding a raw Y14 against it
            // fits a slope between two differently-defined quantities — and
            // that slope is what carries a Y14 effect across to Y30 in the
            // bridged regime. A regression is only a transformation between the
            // things it was fitted on.
            //
            // "Same contrast" therefore means both horizons are contrasted or
            // neither is. Falling back to `0.0` on one side satisfied the
            // sentence above only when the control mean happened to exist;
            // where it did not, the pair was exactly the mismatch this comment
            // forbids. A pair we cannot define consistently is not weak
            // evidence for the slope, it is evidence for a different slope, so
            // it is skipped rather than downweighted.
            // Per-horizon gating: the bridge updates only when both the
            // 14d and 30d horizons are new in this batch. A bridge update
            // from a stale Y14 and a new Y30 (or vice versa) would fit a
            // slope from two differently-timed observations.
            if horizon_is_new(ev.replayed_14d_at, ev.resolved_at)
                && let Some(outcome_y14) = ev.observed_incremental_fans
                && y14_contrast.is_some() == y30_contrast.is_some()
            {
                let paired_y14 = outcome_y14 - y14_contrast.unwrap_or(0.0);
                model.update_bridge(paired_y14, y30_fans);
                bridge_updates += 1;
            }
        }
    }

    // Summary: make a no-op replay visible. If evidence was non-empty but all
    // counters are zero, the replay learned nothing and the bug is upstream
    // (missing outcomes, parse failures, or all rows filtered by the estimand).
    if !evidence.is_empty() {
        tracing::info!(
            evidence_rows = evidence.len(),
            contrast_rows = extra_contrast.len(),
            outcome_updates,
            y14_treatment_updates,
            y30_treatment_updates,
            bridge_updates,
            "evidence replay: posterior update summary"
        );
    }
}

/// Records strategy outcomes into the state-conditioned strategy posterior
/// from growth evidence. The strategy is inferred from the evidence's
/// template_id, and the state (growth_trend, event_proximity) comes from
/// the evidence's context. This is called alongside `apply_evidence_to_model`
/// during the causal model load, so the strategy posterior stays in sync
/// with the causal model's evidence replay.
pub(in crate::autopilot) fn apply_evidence_to_strategy_posterior(
    posterior: &mut crowdrelay_brain::StateConditionedStrategyPosterior,
    evidence: &[crowdrelay_brain::GrowthEvidence],
) {
    use crowdrelay_brain::GrowthStrategy;

    for ev in evidence {
        // Use the recorded strategy when available. Legacy evidence rows
        // (before the strategy field was added) fall back to inferring
        // from the template — a heuristic guess that can be wrong when
        // multiple strategies dispatch the same template.
        let template = extract_template_from_opportunity(&ev.opportunity_id);
        let strategy = ev
            .strategy
            .as_deref()
            .map(|s| s.to_owned())
            .unwrap_or_else(|| {
                GrowthStrategy::infer_from_template(&template)
                    .as_str()
                    .to_owned()
            });
        let growth_trend = ev.context.fan_growth_trend.as_str();
        let event_proximity = match ev.context.days_to_event {
            Some(d) if d <= 7 => "close",
            Some(d) if d <= 30 => "near",
            _ => "far",
        };
        // Use the Y14 incremental outcome as the strategy effectiveness
        // signal. This is the counterfactual-adjusted estimate of how many
        // fans the dispatch produced — exactly what we want to learn which
        // strategies work best.
        if let Some(incremental_fans) = ev.observed_incremental_fans {
            let obs_var = 2.0 * incremental_fans.abs().max(1.0);
            posterior.update(
                &strategy,
                growth_trend,
                event_proximity,
                incremental_fans,
                obs_var,
            );
        }
    }
}

/// Whether the strategy posterior is being extended or rebuilt.
///
/// The distinction is the whole difference between the two call sites and it
/// used to be carried only by a comment. The checkpoint path replays the
/// evidence written since the checkpoint, so it must accumulate onto what is
/// stored. The full-replay path replays *all* evidence, so accumulating onto
/// stored state counts every observation twice — once from the stored posterior
/// and once from the row it was built from — and each full replay does it
/// again. The call site said "from scratch" and the function it called loaded
/// the saved posterior first.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum PosteriorReplay {
    /// Extend the stored posterior with evidence it has not seen.
    Delta,
    /// Rebuild from a skeptical prior; the caller is passing all evidence.
    FromScratch,
}

/// Applies evidence to the state-conditioned strategy posterior in brain state.
///
/// Called alongside `apply_evidence_to_model` during the causal model load so
/// the strategy posterior stays in sync with the causal model's evidence
/// replay. This is the **only** writer of the `strategy_posterior` brain-state
/// key — see [`apply_evidence_to_strategy_posterior`] for why that matters.
pub(super) async fn apply_evidence_to_stored_strategy_posterior(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    evidence: &[crowdrelay_brain::GrowthEvidence],
    replay: PosteriorReplay,
) {
    use crowdrelay_brain::StateConditionedStrategyPosterior;

    let mut posterior = match replay {
        PosteriorReplay::FromScratch => StateConditionedStrategyPosterior::default(),
        PosteriorReplay::Delta => {
            match super::evidence::load_brain_state(repo, workspace_id, "strategy_posterior").await
            {
                Ok(None) => StateConditionedStrategyPosterior::default(),
                Ok(Some((state, _ts))) => {
                    match serde_json::from_value::<StateConditionedStrategyPosterior>(state) {
                        Ok(posterior) => posterior,
                        Err(error) => {
                            // Everything the posterior has learned is in that
                            // row. Starting from a default here and saving the
                            // result below would overwrite it with a delta's
                            // worth of evidence and call that the whole history.
                            // A row we cannot read is not an empty row.
                            tracing::error!(
                                error = %error,
                                workspace_id = %workspace_id.into_uuid(),
                                "stored strategy posterior could not be deserialized; \
                                 skipping the delta rather than overwriting learned \
                                 state with a default"
                            );
                            return;
                        }
                    }
                }
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        workspace_id = %workspace_id.into_uuid(),
                        "could not read the stored strategy posterior; skipping the \
                         delta rather than overwriting learned state with a default"
                    );
                    return;
                }
            }
        }
    };

    apply_evidence_to_strategy_posterior(&mut posterior, evidence);

    match serde_json::to_value(&posterior) {
        Ok(state) => {
            if let Err(error) =
                super::evidence::save_brain_state(repo, workspace_id, "strategy_posterior", &state)
                    .await
            {
                // Best-effort, but not silent: the next cycle will re-derive
                // this from a checkpoint that has already moved past the
                // evidence, so a dropped save is lost learning, not a retry.
                tracing::warn!(
                    error = %error,
                    workspace_id = %workspace_id.into_uuid(),
                    "failed to save the strategy posterior; this cycle's strategy \
                     learning is lost"
                );
            }
        }
        Err(error) => tracing::warn!(
            error = %error,
            "failed to serialize the strategy posterior"
        ),
    }
}

/// Extracts the template_id from an opportunity ID string.
/// Opportunity IDs are formatted as "template:target:action:context_hash".
fn extract_template_from_opportunity(opportunity_id: &Option<String>) -> String {
    opportunity_id
        .as_ref()
        .and_then(|s| s.split(':').next())
        .unwrap_or("unknown")
        .to_owned()
}
