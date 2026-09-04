//! Regime, threshold and bridge invariants for [`CausalModel`].
//!
//! Split out of the parent so the model stays inside the source-size ratchet.
//! These are deterministic in-memory tests: the questions are about which
//! quantity gets ranked and at what evidence, not about persistence.

use super::*;

#[test]
fn causal_model_uses_default_prior_for_unknown_template() {
    let model = CausalModel::new();
    let ctx = DispatchContext::default();
    assert_eq!(
        model.predict("unknown-template", &ctx),
        DEFAULT_EXPECTED_FANS
    );
    assert_eq!(model.confidence("unknown-template"), 0);
}

#[test]
fn causal_model_event_proximity_boosts_prediction() {
    let model = CausalModel::new();
    let ctx_close = DispatchContext {
        days_to_event: Some(5),
        ..Default::default()
    };
    let ctx_far = DispatchContext {
        days_to_event: Some(60),
        ..Default::default()
    };
    // Close event: 2.0 * 1.5 = 3.0
    assert!((model.predict("t", &ctx_close) - 3.0).abs() < 0.01);
    // Far event: no boost
    assert!((model.predict("t", &ctx_far) - 2.0).abs() < 0.01);
}

#[test]
fn causal_model_stagnant_trend_reduces_prediction() {
    let model = CausalModel::new();
    let ctx = DispatchContext {
        fan_growth_trend: GrowthTrend::Stagnant,
        ..Default::default()
    };
    // Stagnant: 2.0 * 0.8 = 1.6
    assert!((model.predict("t", &ctx) - 1.6).abs() < 0.01);
}

#[test]
fn causal_model_updates_from_prediction_error() {
    let mut model = CausalModel::new();
    let prediction = DispatchPrediction {
        template_id: "t".to_owned(),
        expected_new_fans: 2.0,
        ..Default::default()
    };
    // Observed 5 fans — better than expected.
    let outcome = PredictionOutcome::from_observation(prediction, 5.0, 0.0);
    model.update(&outcome);
    // After one Bayesian update, the mean should move toward 5.
    let updated = model.expected_fans("t");
    assert!(
        updated > 2.0 && updated < 5.0,
        "mean should move toward observation, got {updated}"
    );
    assert_eq!(model.confidence("t"), 1);
}

#[test]
fn causal_model_learning_rate_decays_with_confidence() {
    let mut model = CausalModel::new();
    // First update: moves significantly toward observed.
    let p1 = DispatchPrediction {
        template_id: "t".to_owned(),
        expected_new_fans: 2.0,
        ..Default::default()
    };
    model.update(&PredictionOutcome::from_observation(p1, 10.0, 0.0));
    let after_1 = model.expected_fans("t");
    // After many updates with observed=0, the model moves toward 0
    // but each step is smaller (Bayesian precision grows).
    for _ in 0..20 {
        let p = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 10.0,
            ..Default::default()
        };
        model.update(&PredictionOutcome::from_observation(p, 0.0, 0.0));
    }
    let final_val = model.expected_fans("t");
    assert!(final_val < after_1, "model should have moved toward 0");
    assert!(final_val > 0.0, "but never reaches exactly 0");
    assert_eq!(model.confidence("t"), 21);
}

#[test]
fn prediction_error_computes_dopamine_signal() {
    let prediction = DispatchPrediction {
        template_id: "t".to_owned(),
        expected_new_fans: 3.0,
        expected_signal_installs: 1.0,
        ..Default::default()
    };
    let outcome = PredictionOutcome::from_observation(prediction, 7.0, 0.5);
    // Positive fan error: better than expected.
    assert!((outcome.fan_prediction_error - 4.0).abs() < 0.01);
    // Negative signal error: worse than expected.
    assert!((outcome.signal_prediction_error - (-0.5)).abs() < 0.01);
}

#[test]
fn causal_model_variance_starts_at_prior() {
    let model = CausalModel::new();
    // Unmeasured template: the NegBin prior has mean=2.0, dispersion=1.0.
    // α=1.0, β=0.5. Rate variance = α/β² = 1.0/0.25 = 4.0. std = 2.0.
    let std = model.predict_std("t");
    assert!(
        (std - 2.0).abs() < 0.01,
        "prior std should be 2.0, got {std}"
    );
}

#[test]
fn causal_model_variance_shrinks_with_consistent_observations() {
    let mut model = CausalModel::new();
    for _ in 0..10 {
        let p = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 5.0,
            ..Default::default()
        };
        model.update(&PredictionOutcome::from_observation(p, 5.0, 0.0));
    }
    let std = model.predict_std("t");
    assert!(
        std < 1.0,
        "variance should shrink with consistent observations, got std={std}"
    );
}

#[test]
fn causal_model_variance_grows_with_variable_observations() {
    let mut model = CausalModel::new();
    for i in 0..10 {
        let observed = if i % 2 == 0 { 10.0 } else { 0.0 };
        let p = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 5.0,
            ..Default::default()
        };
        model.update(&PredictionOutcome::from_observation(p, observed, 0.0));
    }
    let std = model.predict_std("t");
    assert!(
        std > 0.3,
        "variance should reflect variable observations, got std={std}"
    );
}

#[test]
fn causal_model_signal_prediction_defaults_to_10_percent() {
    let model = CausalModel::new();
    let ctx = DispatchContext::default();
    let signal = model.predict_signal("t", &ctx);
    let fans = model.predict("t", &ctx);
    assert!((signal - fans * 0.1).abs() < 0.01);
}

#[test]
fn causal_model_signal_prediction_learns_independently() {
    let mut model = CausalModel::new();
    let p = DispatchPrediction {
        template_id: "t".to_owned(),
        expected_new_fans: 5.0,
        expected_signal_installs: 0.5,
        ..Default::default()
    };
    model.update(&PredictionOutcome::from_observation(p, 5.0, 3.0));
    let learned = model
        .template_expected_signal
        .get("t")
        .copied()
        .unwrap_or(0.0);
    assert!(
        learned > 1.0,
        "Signal prediction should learn from observed installs, got {learned}"
    );
}

#[test]
fn causal_model_subreddit_type_adjusts_prediction() {
    let mut model = CausalModel::new();
    for _ in 0..10 {
        let p = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 2.0,
            context: DispatchContext {
                subreddit_type: Some("metal".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };
        model.update(&PredictionOutcome::from_observation(p, 8.0, 0.0));
    }
    let ctx_metal = DispatchContext {
        subreddit_type: Some("metal".to_owned()),
        ..Default::default()
    };
    let ctx_none = DispatchContext::default();
    let pred_metal = model.predict("t", &ctx_metal);
    let pred_none = model.predict("t", &ctx_none);
    assert!(
        pred_metal >= pred_none,
        "learned subreddit type should boost prediction: metal={pred_metal}, none={pred_none}"
    );
}

#[test]
fn p_positive_for_effective_template() {
    let mut model = CausalModel::new();
    for _ in 0..10 {
        let p = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 2.0,
            ..Default::default()
        };
        model.update(&PredictionOutcome::from_observation(p, 10.0, 0.0));
    }
    // After 10 observations of 10.0, P(>0) should be very high.
    assert!(
        model.p_positive("t") > 0.99,
        "effective template should have P(>0) ≈ 1"
    );
}

#[test]
fn treatment_effect_falls_back_when_no_data() {
    let model = CausalModel::new();
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    assert!(!stats.use_treatment_effect);
    assert_eq!(stats.treatment_confidence, 0);
    assert!((stats.treatment_effect - 0.0).abs() < 1e-9);
}

#[test]
fn treatment_effect_falls_back_below_threshold() {
    let mut model = CausalModel::new();
    // Only 2 observations — below the hysteresis floor (5-2=3).
    for _ in 0..2 {
        model.update_treatment_effect_for_target(
            "t",
            None,
            None,
            5.0,
            1.0,
            crate::evidence::EvidenceQuality::RandomizedHoldout,
        );
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    assert!(!stats.use_treatment_effect);
    assert_eq!(stats.treatment_confidence, 2);
}

#[test]
fn treatment_effect_used_above_threshold() {
    let mut model = CausalModel::new();
    // 10 observations — above MIN_TREATMENT_CONFIDENCE (5).
    for _ in 0..10 {
        model.update_treatment_effect_for_target(
            "t",
            None,
            None,
            5.0,
            1.0,
            crate::evidence::EvidenceQuality::RandomizedHoldout,
        );
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    assert!(stats.use_treatment_effect);
    assert_eq!(stats.treatment_confidence, 10);
    assert!(stats.treatment_effect > 2.0);
}

#[test]
fn treatment_effect_can_be_negative() {
    let mut model = CausalModel::new();
    for _ in 0..10 {
        model.update_treatment_effect_for_target(
            "bad",
            None,
            None,
            -3.0,
            1.0,
            crate::evidence::EvidenceQuality::RandomizedHoldout,
        );
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("bad", &ctx);
    assert!(stats.use_treatment_effect);
    assert!(stats.treatment_effect < -1.0);
}

#[test]
fn treatment_effect_respects_subreddit_type() {
    let mut model = CausalModel::new();
    // Observe high values for "metal" subreddit type.
    for _ in 0..10 {
        model.update_treatment_effect("t", Some("metal"), 8.0, 1.0);
    }
    // Observe low values for "pop" subreddit type to move the global down.
    for _ in 0..10 {
        model.update_treatment_effect("t", Some("pop"), 0.0, 1.0);
    }
    let ctx_metal = DispatchContext {
        subreddit_type: Some("metal".to_owned()),
        ..Default::default()
    };
    let ctx_none = DispatchContext::default();
    let stats_metal = model.predict_stats_with_treatment("t", &ctx_metal);
    let stats_none = model.predict_stats_with_treatment("t", &ctx_none);
    assert!(
        stats_metal.treatment_effect > stats_none.treatment_effect,
        "metal subreddit type should boost treatment effect above global, got metal={} none={}",
        stats_metal.treatment_effect,
        stats_none.treatment_effect
    );
}

// ── Y30 North Star + Y14 bridge tests (P0.3) ─────────────────────────

#[test]
fn y30_preferred_when_confident() {
    let mut model = CausalModel::new();
    // Build Y30 confidence above MIN_TREATMENT_CONFIDENCE.
    for _ in 0..10 {
        model.update_treatment_effect_y30_for_target(
            "t",
            None,
            None,
            5.0,
            1.0,
            crate::evidence::EvidenceQuality::RandomizedHoldout,
        );
    }
    // Also build Y14 confidence (but Y30 should be preferred).
    for _ in 0..10 {
        model.update_treatment_effect_for_target(
            "t",
            None,
            None,
            3.0,
            1.0,
            crate::evidence::EvidenceQuality::RandomizedHoldout,
        );
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    assert!(
        stats.uses_y30,
        "should use Y30 when Y30 confidence >= threshold"
    );
    assert!(
        stats.use_treatment_effect,
        "should use treatment effect (Y30 is confident)"
    );
    // The treatment effect should be the Y30 value (5.0), not Y14 (3.0).
    assert!(
        (stats.treatment_effect - 5.0).abs() < 2.0,
        "treatment effect should be close to Y30 (5.0), got {}",
        stats.treatment_effect
    );
}

#[test]
fn y14_bridge_inflates_uncertainty() {
    let mut model = CausalModel::new();
    // Build Y14 confidence but NOT Y30 confidence.
    for _ in 0..10 {
        model.update_treatment_effect_for_target(
            "t",
            None,
            None,
            5.0,
            1.0,
            crate::evidence::EvidenceQuality::RandomizedHoldout,
        );
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    assert!(
        stats.use_treatment_effect,
        "should use Y14 treatment effect (confident)"
    );
    assert!(!stats.uses_y30, "should not use Y30 (not confident yet)");
    // The treatment_std should be inflated by the bridge.
    // Compare against the raw Y14 std.
    let (_raw_tau, raw_std, _) = model.treatment_effects.predict_stats("t", None);
    assert!(
        stats.treatment_std > raw_std,
        "bridge should inflate uncertainty, got stats_std={:.4} raw_std={:.4}",
        stats.treatment_std,
        raw_std
    );
}

#[test]
fn bridge_learns_y14_y30_relationship() {
    let mut model = CausalModel::new();
    // True relationship: Y30 = 0.5 * Y14 (durable fans are half of incremental).
    // Update the bridge with paired observations.
    for y14 in [2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0] {
        let y30 = 0.5 * y14;
        model.update_bridge(y14, y30);
    }
    // The bridge slope should have moved toward 0.5.
    let slope = model.bridge.slope();
    assert!(
        (slope - 0.5).abs() < 0.3,
        "bridge slope should converge toward 0.5, got {slope:.3}"
    );
    // Predict Y30 from Y14=10 → should be ~5.0.
    let (predicted, _) = model.bridge.predict(10.0);
    assert!(
        (predicted - 5.0).abs() < 2.0,
        "bridge should predict Y30≈5.0 from Y14=10, got {predicted:.3}"
    );
}

#[test]
fn y30_falls_back_to_outcome_when_neither_confident() {
    let model = CausalModel::new();
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    assert!(
        !stats.use_treatment_effect,
        "should fall back to outcome model when neither Y14 nor Y30 is confident"
    );
    assert!(!stats.uses_y30);
}

// ── P(τ > δ) meaningful-effect tests (P1.6) ──────────────────────────

#[test]
fn p_meaningful_effect_is_high_for_strong_template() {
    let mut model = CausalModel::new();
    // Build Y14 confidence with strong positive effects.
    for _ in 0..20 {
        model.update_treatment_effect("t", None, 10.0, 1.0);
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    // P(τ > 1.0) should be high — the mean is ~10 and std is small.
    assert!(
        stats.p_meaningful_effect > 0.9,
        "P(τ > δ) should be high for strong template, got {}",
        stats.p_meaningful_effect
    );
}

#[test]
fn p_meaningful_effect_is_low_for_negative_template() {
    let mut model = CausalModel::new();
    for _ in 0..20 {
        model.update_treatment_effect("bad", None, -5.0, 1.0);
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("bad", &ctx);
    // P(τ > 1.0) should be very low — the mean is ~-5.
    assert!(
        stats.p_meaningful_effect < 0.01,
        "P(τ > δ) should be low for negative template, got {}",
        stats.p_meaningful_effect
    );
}

#[test]
fn p_meaningful_effect_is_moderate_for_uncertain_template() {
    let mut model = CausalModel::new();
    // Mean near the threshold, high variance.
    for _ in 0..5 {
        model.update_treatment_effect("t", None, 2.0, 5.0);
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    // P(τ > 1.0) should be somewhere in the middle — not extreme.
    assert!(
        stats.p_meaningful_effect > 0.3 && stats.p_meaningful_effect < 0.9,
        "P(τ > δ) should be moderate for uncertain template, got {}",
        stats.p_meaningful_effect
    );
}

// ── Hysteresis tests (P1.7) ──────────────────────────────────────────

/// The stated minimum is the whole threshold, with nothing underneath it.
///
/// A third branch used to accept three or four observations, calling the
/// lower bar hysteresis. Hysteresis is a function of the regime you were
/// last in, and this function is handed only posteriors, so what the branch
/// really did was let a fresh model rank on a treatment effect two
/// observations before the minimum said it could be trusted — and, having
/// no memory, never treat that as a temporary state.
#[test]
fn a_fresh_model_reaches_the_treatment_regime_only_at_the_stated_minimum() {
    let ctx = DispatchContext::default();
    let switched_after = |observations: u32| {
        let mut model = CausalModel::new();
        for _ in 0..observations {
            model.update_treatment_effect_for_target(
                "t",
                None,
                None,
                5.0,
                1.0,
                crate::evidence::EvidenceQuality::RandomizedHoldout,
            );
        }
        model
            .predict_stats_with_treatment("t", &ctx)
            .use_treatment_effect
    };
    for observations in 0..MIN_TREATMENT_CONFIDENCE {
        assert!(
            !switched_after(observations),
            "{observations} observations is below the minimum of \
                 {MIN_TREATMENT_CONFIDENCE} and must rank on the outcome model"
        );
    }
    assert!(
        switched_after(MIN_TREATMENT_CONFIDENCE),
        "the minimum must be reachable, or the treatment effect is unusable"
    );
}

/// Each horizon carries its own threshold.
#[test]
fn the_two_horizons_cross_the_threshold_independently() {
    let ctx = DispatchContext::default();
    let quality = crate::evidence::EvidenceQuality::RandomizedHoldout;

    // Y14 alone: treatment regime, bridged rather than direct.
    let mut y14_only = CausalModel::new();
    for _ in 0..MIN_TREATMENT_CONFIDENCE {
        y14_only.update_treatment_effect_for_target("t", None, None, 5.0, 1.0, quality);
    }
    let stats = y14_only.predict_stats_with_treatment("t", &ctx);
    assert!(stats.use_treatment_effect, "Y14 alone crosses on its own");
    assert!(!stats.uses_y30, "but it is not a direct Y30 estimate");

    // Y30 alone: direct, and Y14's own confidence stays at zero.
    let mut y30_only = CausalModel::new();
    for _ in 0..MIN_TREATMENT_CONFIDENCE {
        y30_only.update_treatment_effect_y30_for_target("t", None, None, 4.0, 1.0, quality);
    }
    let stats = y30_only.predict_stats_with_treatment("t", &ctx);
    assert!(stats.uses_y30, "Y30 alone crosses on its own");
    assert_eq!(
        stats.treatment_confidence_y30, MIN_TREATMENT_CONFIDENCE,
        "and reports its own count"
    );
}

/// The bridge carries the Y14 effect across to Y30 instead of being
/// consulted only for uncertainty.
///
/// The bridged branch took the bridge's variance and discarded its mean, so
/// a bridge that had learned "a fourteen-day effect is worth half of it at
/// thirty days" widened the interval and left the point estimate saying the
/// two horizons were the same number.
///
/// The transformation is the bridge's own prediction because the pairs it
/// is fitted on are already effects: `observed_incremental_fans` and
/// `durable_fans_30d` are both stored counterfactual-adjusted, so the
/// regression is effect-on-effect and its conditional expectation is the
/// Y30 effect. In a regression of raw counts the intercept would cancel out
/// of a difference and only the slope would carry across; that is not the
/// fit being used here.
#[test]
fn the_bridge_slope_carries_the_treatment_effect_across_horizons() {
    let ctx = DispatchContext::default();
    let quality = crate::evidence::EvidenceQuality::RandomizedHoldout;
    let mut model = CausalModel::new();
    for _ in 0..MIN_TREATMENT_CONFIDENCE {
        model.update_treatment_effect_for_target("t", None, None, 4.0, 1.0, quality);
    }
    let before = model.predict_stats_with_treatment("t", &ctx);
    assert!(
        before.use_treatment_effect && !before.uses_y30,
        "bridged regime"
    );

    // Teach the bridge a halving relationship through the origin, so the
    // slope is well away from the prior's 1.0 and the intercept is not.
    for k in 1..=40 {
        let y14 = f64::from(k % 8) + 1.0;
        model.update_bridge(y14, y14 * 0.5);
    }
    let after = model.predict_stats_with_treatment("t", &ctx);

    assert!(
        after.treatment_effect < before.treatment_effect * 0.75,
        "a halving bridge must pull the estimate down, {} to {}",
        before.treatment_effect,
        after.treatment_effect
    );
    assert!(
        after.treatment_effect > 0.0,
        "and must not invert the sign, got {}",
        after.treatment_effect
    );
}

/// A bridge fitted through the origin invents no effect where there is none.
#[test]
fn a_bridge_through_the_origin_maps_no_effect_to_no_effect() {
    let mut model = CausalModel::new();
    for k in 1..=40 {
        let y14 = f64::from(k % 8) + 1.0;
        model.update_bridge(y14, y14 * 0.5);
    }
    let (bridged_zero, _) = model.bridge.predict(0.0);
    assert!(
        bridged_zero.abs() < 0.3,
        "no fourteen-day effect must stay no thirty-day effect, got {bridged_zero}"
    );
}

/// The bridge's own uncertainty is still added to the Y14 posterior's.
#[test]
fn the_bridged_estimate_keeps_the_bridge_uncertainty() {
    let ctx = DispatchContext::default();
    let quality = crate::evidence::EvidenceQuality::RandomizedHoldout;
    let mut model = CausalModel::new();
    for _ in 0..MIN_TREATMENT_CONFIDENCE {
        model.update_treatment_effect_for_target("t", None, None, 4.0, 1.0, quality);
    }
    let stats = model.predict_stats_with_treatment("t", &ctx);
    let (_, raw_std, _) = model.treatment_effects.predict_stats("t", None);
    assert!(
        stats.treatment_std > raw_std,
        "bridged std must exceed the raw Y14 std, {} vs {}",
        stats.treatment_std,
        raw_std
    );
}

/// The meaningful-effect probability describes the quantity being ranked.
///
/// In the bridged regime it used to report P(τ_y14 > δ) — a statement about
/// the fourteen-day effect, carrying none of the bridge's uncertainty —
/// while the decision was being made on the bridged thirty-day estimate.
#[test]
fn the_meaningful_effect_probability_describes_the_bridged_estimate() {
    let ctx = DispatchContext::default();
    let quality = crate::evidence::EvidenceQuality::RandomizedHoldout;
    let mut model = CausalModel::new();
    for _ in 0..MIN_TREATMENT_CONFIDENCE {
        model.update_treatment_effect_for_target("t", None, None, 4.0, 1.0, quality);
    }
    for k in 1..=40 {
        let y14 = f64::from(k % 8) + 1.0;
        model.update_bridge(y14, y14 * 0.5);
    }
    let stats = model.predict_stats_with_treatment("t", &ctx);
    assert!(
        stats.use_treatment_effect && !stats.uses_y30,
        "bridged regime"
    );

    // Exactly reconstructible from the pair the decision is made on: the
    // bridged posterior is a linear map of a Normal plus independent
    // bridge noise, so it is Normal too.
    let z = (MEANINGFUL_EFFECT_THRESHOLD - stats.treatment_effect) / stats.treatment_std.max(0.1);
    let expected = 1.0 - crate::bayesian::normal_cdf(z);
    assert!(
        (stats.p_meaningful_effect - expected).abs() < 1e-9,
        "p_meaningful must follow the bridged estimate, got {} expected {expected}",
        stats.p_meaningful_effect
    );
    // And it must differ from the Y14-posterior answer it used to report,
    // or the correction would be untestable.
    let y14_answer =
        model
            .treatment_effects
            .p_meaningful_effect("t", None, MEANINGFUL_EFFECT_THRESHOLD);
    assert!(
        (stats.p_meaningful_effect - y14_answer).abs() > 1e-6,
        "the halved bridge must move the probability away from the Y14 answer"
    );
}

/// The Y30-direct regime is untouched by the bridge.
#[test]
fn the_direct_y30_regime_ignores_the_bridge() {
    let ctx = DispatchContext::default();
    let quality = crate::evidence::EvidenceQuality::RandomizedHoldout;
    let mut model = CausalModel::new();
    for _ in 0..MIN_TREATMENT_CONFIDENCE {
        model.update_treatment_effect_y30_for_target("t", None, None, 4.0, 1.0, quality);
    }
    let before = model.predict_stats_with_treatment("t", &ctx);
    assert!(before.uses_y30, "direct regime");
    for k in 1..=40 {
        let y14 = f64::from(k % 8) + 1.0;
        model.update_bridge(y14, y14 * 0.5);
    }
    let after = model.predict_stats_with_treatment("t", &ctx);
    assert_eq!(
        before.treatment_effect, after.treatment_effect,
        "a direct Y30 estimate must not move when the bridge learns"
    );
    assert_eq!(
        before.treatment_std, after.treatment_std,
        "nor may its uncertainty"
    );
}

#[test]
fn weak_evidence_does_not_drop_a_confident_template_below_the_threshold() {
    let mut model = CausalModel::new();
    // Build Y14 confidence to exactly MIN_TREATMENT_CONFIDENCE (5).
    for _ in 0..5 {
        model.update_treatment_effect_for_target(
            "t",
            None,
            None,
            5.0,
            1.0,
            crate::evidence::EvidenceQuality::RandomizedHoldout,
        );
    }
    let ctx = DispatchContext::default();
    let stats = model.predict_stats_with_treatment("t", &ctx);
    assert!(
        stats.use_treatment_effect,
        "should use treatment effect at MIN_TREATMENT_CONFIDENCE"
    );
    // One more observation, unqualified and therefore observational: it
    // lowers the mean and adds a tenth of an observation. Confidence stays
    // at five, so the regime does not move. Nothing here is hysteresis —
    // the threshold simply was not crossed downward.
    model.update_treatment_effect("t", None, 0.0, 1.0);
    let stats2 = model.predict_stats_with_treatment("t", &ctx);
    assert!(
        stats2.use_treatment_effect,
        "a weak extra observation must not push a confident template back"
    );
}

// ── Checkpoint compatibility ──────────────────────────────────────────
//
// Old checkpoints in `viryaos_brain_state` may carry fields that were
// removed from `CausalModel` (reach_model, funnel, rich_state_transitions).
// Serde ignores unknown fields, so deserialization must keep working —
// a shape change here is exactly the failure class that produced the
// autopilot control-overview 500.

#[test]
fn deserializes_legacy_checkpoint_with_removed_fields() {
    let mut json = serde_json::to_value(CausalModel::new()).unwrap();
    let obj = json.as_object_mut().unwrap();
    obj.insert(
        "reach_model".to_owned(),
        serde_json::json!({ "conversions": {}, "exposures": {} }),
    );
    obj.insert("funnel".to_owned(), serde_json::json!({}));
    obj.insert("rich_state_transitions".to_owned(), serde_json::json!({}));

    let model: CausalModel = serde_json::from_value(json)
        .expect("legacy checkpoint with removed fields must still deserialize");
    let ctx = DispatchContext::default();
    assert!(model.predict("t", &ctx) > 0.0);
}

// ── Prediction-error isolation invariant ──
//
// Prediction error (fan_prediction_error, signal_prediction_error) is
// a dopamine signal that updates beliefs/calibration. It MUST NOT
// directly manufacture DecisionValue, economic value, portfolio value,
// or goal value.
//
// Two complementary tests prove this invariant:
//
// 1. prediction_error_does_not_manufacture_decision_value — single
//    update, exact equality through the FULL pipeline
//    (predict_stats_with_treatment, which includes calibration). A
//    single update hasn't diverged calibration yet, so the full
//    pipeline output must be identical.
//
// 2. prediction_error_only_updates_belief_not_economics — multi-update,
//    exact equality at the POSTERIOR boundary (predict_stats, raw
//    hierarchical posterior without calibration). After many updates,
//    calibration legitimately differs (it tracks prediction bias),
//    but the raw posterior must remain identical because it learns
//    from the observed value, not the error.

#[test]
fn prediction_error_does_not_manufacture_decision_value() {
    let mut model_a = CausalModel::new();
    let mut model_b = CausalModel::new();

    // Same template, same context, same observed fans.
    // Different expected fans → different prediction errors.
    let template = "community.engage.request";
    let ctx = DispatchContext::default();

    // Model A: expected 2, observed 5 → error = +3 (surprise reward)
    let pred_a = DispatchPrediction {
        template_id: template.to_owned(),
        expected_new_fans: 2.0,
        expected_signal_installs: 0.0,
        context: ctx.clone(),
        target_key: None,
        creative_family: None,
    };
    let outcome_a = PredictionOutcome::from_observation(pred_a, 5.0, 0.0);
    assert!(
        outcome_a.fan_prediction_error > 0.0,
        "model A should have positive prediction error"
    );

    // Model B: expected 5, observed 5 → error = 0 (no surprise)
    let pred_b = DispatchPrediction {
        template_id: template.to_owned(),
        expected_new_fans: 5.0,
        expected_signal_installs: 0.0,
        context: ctx.clone(),
        target_key: None,
        creative_family: None,
    };
    let outcome_b = PredictionOutcome::from_observation(pred_b, 5.0, 0.0);
    assert!(
        outcome_b.fan_prediction_error.abs() < 1e-9,
        "model B should have zero prediction error"
    );

    // Update both models with the same observed outcome.
    model_a.update(&outcome_a);
    model_b.update(&outcome_b);

    // The treatment-aware stats (which feed DecisionValue) must be
    // identical — both models saw the same observed fan count.
    let stats_a = model_a.predict_stats_with_treatment(template, &ctx);
    let stats_b = model_b.predict_stats_with_treatment(template, &ctx);

    assert_eq!(
        stats_a.expected_fans, stats_b.expected_fans,
        "expected_fans must be identical regardless of prediction error"
    );
    assert_eq!(
        stats_a.predict_std, stats_b.predict_std,
        "predict_std must be identical regardless of prediction error"
    );
    assert_eq!(
        stats_a.confidence, stats_b.confidence,
        "confidence must be identical regardless of prediction error"
    );
    assert_eq!(
        stats_a.treatment_effect, stats_b.treatment_effect,
        "treatment_effect must be identical regardless of prediction error"
    );
    assert_eq!(
        stats_a.treatment_confidence, stats_b.treatment_confidence,
        "treatment_confidence must be identical regardless of prediction error"
    );
}

#[test]
fn prediction_error_only_updates_belief_not_economics() {
    // Stronger version: feed many observations with different prediction
    // errors but the same observed values. The model's raw posterior
    // (before calibration/treatment presentation) must be identical,
    // regardless of the prediction error history.
    //
    // We compare predict_stats() — the raw hierarchical posterior
    // (mean, variance, confidence) — NOT predict_stats_with_treatment(),
    // which applies calibration correction that legitimately differs
    // when prediction history differs. The invariant is at the
    // posterior-update boundary, not the final output boundary.
    let mut model_a = CausalModel::new();
    let mut model_b = CausalModel::new();
    let template = "t";
    let ctx = DispatchContext::default();

    // Model A: consistently over-predicts (expected=10, observed=5)
    // → large negative prediction error every time
    for _ in 0..10 {
        let pred = DispatchPrediction {
            template_id: template.to_owned(),
            expected_new_fans: 10.0,
            ..Default::default()
        };
        model_a.update(&PredictionOutcome::from_observation(pred, 5.0, 0.0));
    }

    // Model B: consistently under-predicts (expected=0, observed=5)
    // → large positive prediction error every time
    for _ in 0..10 {
        let pred = DispatchPrediction {
            template_id: template.to_owned(),
            expected_new_fans: 0.0,
            ..Default::default()
        };
        model_b.update(&PredictionOutcome::from_observation(pred, 5.0, 0.0));
    }

    // Both models should have the same raw posterior — they both
    // observed 5.0 fans ten times. The prediction error (which differs
    // wildly) must not influence the outcome model's belief.
    let (expected_a, std_a, conf_a) = model_a.predict_stats(template, &ctx);
    let (expected_b, std_b, conf_b) = model_b.predict_stats(template, &ctx);

    assert_eq!(
        conf_a, conf_b,
        "both models should have the same confidence (10 observations)"
    );
    assert_eq!(
        expected_a, expected_b,
        "raw posterior expected_fans must be identical regardless of prediction error"
    );
    assert_eq!(
        std_a, std_b,
        "raw posterior predict_std must be identical regardless of prediction error"
    );
}
