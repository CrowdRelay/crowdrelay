//! Behaviour of the portfolio optimizer.
//!
//! Split from `portfolio.rs` so the algorithm stays inside the source-size
//! ratchet. Deterministic in-memory tests: the questions are about which
//! candidate wins, what the adjustment ledger says it cost, and whether the
//! ordering depends on the order candidates arrived in.

use super::*;

use super::{
    DecisionMode, PortfolioCandidate, PortfolioConfig, PortfolioOptimizer, RejectionReason,
};
use crate::causal_model::DispatchContext;
use crate::opportunity::OpportunityAction;
use crate::resource_cost::ResourceCost;
use std::collections::HashSet;

fn make_candidate(
    template: &str,
    target: &str,
    expected_fans: f64,
    audience: &str,
) -> PortfolioCandidate {
    let ctx = DispatchContext::default();
    let decision_value = DecisionValue {
        expected_incremental_y30: expected_fans,
        uncertainty: 0.0,
        p_meaningful_effect: 0.0,
        estimation_regime: crate::decision_value::EstimationRegime::OutcomeModel,
        evidence_quality: crate::evidence::EvidenceQuality::Observational,
        sample_size: 0,
        uses_y30: false,
        bridge_confidence: 0,
        bridge_is_reliable: false,
        contamination: 0.0,
        calibration_bias: 0.0,
        resource_cost: ResourceCost::configured(1.0),
        pragmatic_value: expected_fans,
        risk_penalty: None,
        opportunity_cost: 0.0,
        decision_mode: DecisionMode::Exploit,
    };
    PortfolioCandidate {
        opportunity_id: OpportunityId::new(template, target, OpportunityAction::Post, &ctx),
        generation_signal: None,
        audience_key: audience.to_owned(),
        source_context: "GrowthIntelligence".to_owned(),
        action_key: format!("action:{template}:{target}"),
        is_experimental: false,
        decision_value,
    }
}

/// Turns a candidate into a `Y14Bridged` estimate with a stated bridge
/// reliability, leaving every economic term alone.
fn bridged(mut candidate: PortfolioCandidate, reliable: bool) -> PortfolioCandidate {
    candidate.decision_value.estimation_regime =
        crate::decision_value::EstimationRegime::Y14Bridged;
    candidate.decision_value.bridge_is_reliable = reliable;
    candidate.decision_value.bridge_confidence = if reliable { 15 } else { 0 };
    candidate
}

/// The bridge penalty is economically live, and it applies to one regime.
///
/// `DecisionValue::total()` is the posterior mean alone, which reads as
/// "epistemic facts do not move ranking" — and for `uncertainty`,
/// `evidence_quality` and `sample_size` that is true. `bridge_is_reliable`
/// is the exception: the optimizer docks an uncalibrated `Y14Bridged`
/// candidate 20% at the marginal, because such an estimate is a Y14 number
/// wearing a Y30 label.
///
/// It was reachable by no test. The shared fixture builds `OutcomeModel`
/// candidates, so the branch never fired, and a hand-tuned coefficient
/// that reorders candidates had neither a test nor an accurate comment.
/// Pin both halves: that it bites, and that it bites nothing else.
#[test]
fn an_uncalibrated_bridge_is_discounted_and_only_that_regime_is() {
    let optimizer = PortfolioOptimizer::new(PortfolioConfig {
        max_dispatches: 1,
        ..PortfolioConfig::default()
    });

    // Same expected fans, different audiences so neither overlaps the
    // other. The only difference between the two is the bridge.
    let contest = |challenger: PortfolioCandidate| {
        let selection = optimizer.select(vec![
            challenger,
            make_candidate("defender", "t2", 10.0, "audience-b"),
        ]);
        selection
            .selected
            .first()
            .map(|c| c.opportunity_id.to_string())
            .unwrap_or_else(|| "none".to_owned())
    };

    let uncalibrated = bridged(
        make_candidate("challenger", "t1", 11.0, "audience-a"),
        false,
    );
    let calibrated = bridged(make_candidate("challenger", "t1", 11.0, "audience-a"), true);
    let observational = make_candidate("challenger", "t1", 11.0, "audience-a");

    // 11.0 x 0.8 = 8.8, which loses to 10.0. Without the penalty it wins.
    assert!(
        !contest(uncalibrated).contains("challenger"),
        "an uncalibrated Y14Bridged candidate must be discounted enough to              lose to a lower-mean candidate it would otherwise beat"
    );
    assert!(
        contest(calibrated).contains("challenger"),
        "a calibrated bridge carries no penalty, so the higher mean wins"
    );
    assert!(
        contest(observational).contains("challenger"),
        "the penalty is scoped to Y14Bridged — an OutcomeModel candidate              with an unreliable-bridge flag must not be docked for it"
    );
}

/// The recorded breakdown reconstructs the number the optimizer ranked on.
///
/// The struct's own arithmetic is tested in `adjustments.rs`. This is the
/// harder half: that the ledger the *optimizer* records is the ledger of
/// the computation it actually performed, for every selected candidate,
/// including one that shares an audience (overlap and fatigue both bite)
/// and one carrying an uncalibrated bridge.
///
/// A breakdown that does not reach the marginal is worse than none: it
/// looks like an explanation, and a reader who trusts it concludes the
/// wrong thing about why a candidate won or lost.
#[test]
fn the_recorded_breakdown_explains_the_marginal_the_optimizer_used() {
    let optimizer = PortfolioOptimizer::new(PortfolioConfig {
        max_dispatches: 4,
        ..PortfolioConfig::default()
    });
    let selection = optimizer.select(vec![
        make_candidate("a", "t1", 12.0, "shared-audience"),
        // Same audience: the second one pays overlap and fatigue.
        make_candidate("b", "t2", 11.0, "shared-audience"),
        // Its own audience, but an uncalibrated bridge.
        bridged(make_candidate("c", "t3", 10.0, "own-audience"), false),
    ]);

    assert_eq!(
        selection.selected.len(),
        3,
        "the fixture must select all three, or it is testing something else"
    );
    assert_eq!(
        selection.marginal_adjustments.len(),
        selection.selected.len(),
        "every selected candidate must carry a breakdown"
    );

    let mut saw_overlap = false;
    let mut saw_bridge = false;
    for candidate in &selection.selected {
        let key = candidate.opportunity_id.to_string();
        let adjustments = selection
            .marginal_adjustments
            .get(&key)
            .unwrap_or_else(|| panic!("no breakdown recorded for {key}"));
        assert!(
            (adjustments.intrinsic_y30 - candidate.decision_value.total()).abs() < 1e-9,
            "the breakdown's intrinsic must be the candidate's own total: {} vs {}",
            adjustments.intrinsic_y30,
            candidate.decision_value.total()
        );
        let summed = adjustments.intrinsic_y30
            + adjustments.overlap_adjustment
            + adjustments.fatigue_adjustment
            + adjustments.bridge_adjustment;
        assert!(
            (summed - adjustments.marginal_y30).abs() < 1e-9,
            "breakdown for {key} sums to {summed}, marginal is {}",
            adjustments.marginal_y30
        );
        assert!(
            adjustments.overlap_adjustment <= 0.0
                && adjustments.fatigue_adjustment <= 0.0
                && adjustments.bridge_adjustment <= 0.0,
            "a portfolio adjustment may only reduce value; {key} has {adjustments:?}"
        );
        saw_overlap |= adjustments.overlap_adjustment < 0.0;
        saw_bridge |= adjustments.bridge_adjustment < 0.0;
    }
    assert!(
        saw_overlap,
        "the fixture must actually exercise overlap, or the sum check is trivial"
    );
    assert!(
        saw_bridge,
        "the fixture must actually exercise the bridge discount"
    );

    // The aggregate is the sum of the marginals, which is what its doc
    // comment now says and what its name does not.
    let summed_marginals: f64 = selection
        .marginal_adjustments
        .values()
        .map(|a| a.marginal_y30)
        .sum();
    assert!(
        (selection.total_expected_fans - summed_marginals).abs() < 1e-9,
        "total_expected_fans is the sum of marginals: {} vs {summed_marginals}",
        selection.total_expected_fans
    );
}

#[test]
fn empty_pool_returns_do_nothing() {
    let optimizer = PortfolioOptimizer::default();
    let result = optimizer.select(vec![]);
    assert!(result.do_nothing);
    assert!(result.selected.is_empty());
}

#[test]
fn single_candidate_is_selected() {
    let optimizer = PortfolioOptimizer::default();
    let result = optimizer.select(vec![make_candidate("a", "x", 5.0, "aud1")]);
    assert!(!result.do_nothing);
    assert_eq!(result.selected.len(), 1);
    assert!((result.total_expected_fans - 5.0).abs() < 0.01);
}

#[test]
fn higher_expected_fans_selected_first() {
    let optimizer = PortfolioOptimizer::default();
    let result = optimizer.select(vec![
        make_candidate("a", "x", 2.0, "aud1"),
        make_candidate("b", "y", 10.0, "aud2"),
    ]);
    assert_eq!(result.selected[0].opportunity_id.template_id, "b");
}

#[test]
fn budget_limit_caps_selection() {
    let config = PortfolioConfig {
        max_dispatches: 2,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let result = optimizer.select(vec![
        make_candidate("a", "x", 5.0, "aud1"),
        make_candidate("b", "y", 4.0, "aud2"),
        make_candidate("c", "z", 3.0, "aud3"),
    ]);
    assert_eq!(result.selected.len(), 2);
    assert_eq!(result.rejected.len(), 1);
    assert_eq!(
        result.rejected[0].reason,
        RejectionReason::MaxDispatchesReached
    );
}

#[test]
fn audience_overlap_reduces_marginal_value() {
    let config = PortfolioConfig {
        max_dispatches: 5,
        audience_overlap_penalty: 0.5,
        fatigue_decay: 1.0, // disable fatigue to isolate overlap
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    // Two candidates targeting the same audience.
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "same_audience"),
        make_candidate("b", "y", 8.0, "same_audience"),
    ]);
    // First candidate: marginal = 10.0 (no overlap).
    // Second candidate: marginal = 8.0 * (1 - 0.5 * 1) * 1.0 = 4.0.
    // Both should be selected (4.0 > min_marginal_value).
    assert_eq!(result.selected.len(), 2);
    // Total = 10.0 + 4.0 = 14.0
    assert!((result.total_expected_fans - 14.0).abs() < 0.01);
}

#[test]
fn high_overlap_penalty_rejects_second_candidate() {
    let config = PortfolioConfig {
        max_dispatches: 5,
        audience_overlap_penalty: 1.0,
        fatigue_decay: 1.0, // disable fatigue to isolate overlap
        min_marginal_value: 0.5,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "same_audience"),
        make_candidate("b", "y", 8.0, "same_audience"),
    ]);
    // First: marginal = 10.0. Second: marginal = 8.0 * 0.0 * 1.0 = 0.0 < 0.5.
    assert_eq!(result.selected.len(), 1);
    assert_eq!(result.rejected.len(), 1);
}

#[test]
fn different_audiences_have_no_overlap_penalty() {
    let optimizer = PortfolioOptimizer::default();
    let result = optimizer.select(vec![
        make_candidate("a", "x", 5.0, "aud1"),
        make_candidate("b", "y", 5.0, "aud2"),
    ]);
    assert_eq!(result.selected.len(), 2);
    // No overlap: total = 5.0 + 5.0 = 10.0
    assert!((result.total_expected_fans - 10.0).abs() < 0.01);
}

#[test]
fn negative_marginal_value_triggers_do_nothing() {
    let config = PortfolioConfig {
        min_marginal_value: 1.0,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let result = optimizer.select(vec![make_candidate("a", "x", 0.5, "aud1")]);
    assert!(result.do_nothing);
    assert!(result.selected.is_empty());
}

#[test]
fn greedy_picks_highest_marginal_not_highest_absolute() {
    let config = PortfolioConfig {
        max_dispatches: 2,
        audience_overlap_penalty: 0.5,
        fatigue_decay: 1.0, // disable fatigue to isolate overlap
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    // Candidate A: 10 fans, audience "shared"
    // Candidate B: 8 fans, audience "unique"
    // Candidate C: 9 fans, audience "shared"
    //
    // Greedy step 1: pick A (10.0 marginal).
    // Greedy step 2: B has marginal 8.0 (no overlap), C has marginal
    //   9.0 * (1 - 0.5) * 1.0 = 4.5 (overlap with A). Pick B (8.0 > 4.5).
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "shared"),
        make_candidate("b", "y", 8.0, "unique"),
        make_candidate("c", "z", 9.0, "shared"),
    ]);
    assert_eq!(result.selected.len(), 2);
    assert_eq!(result.selected[0].opportunity_id.template_id, "a");
    assert_eq!(result.selected[1].opportunity_id.template_id, "b");
}

#[test]
fn total_expected_fans_uses_marginal_not_absolute() {
    let config = PortfolioConfig {
        max_dispatches: 5,
        audience_overlap_penalty: 0.3,
        fatigue_decay: 1.0, // disable fatigue to isolate overlap
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "shared"),
        make_candidate("b", "y", 8.0, "shared"),
    ]);
    // Total = 10.0 + 8.0 * (1 - 0.3) * 1.0 = 10.0 + 5.6 = 15.6
    assert!((result.total_expected_fans - 15.6).abs() < 0.01);
}

#[test]
fn portfolio_config_defaults_are_sensible() {
    let config = PortfolioConfig::default();
    assert_eq!(config.max_dispatches, 5);
    assert_eq!(config.cost_budget, 0.0);
    assert!((config.audience_overlap_penalty - 0.3).abs() < 0.01);
    assert!((config.fatigue_decay - 0.9).abs() < 0.01);
    assert!((config.min_marginal_value - 0.1).abs() < 0.01);
}

#[test]
fn cost_budget_limits_selection() {
    let config = PortfolioConfig {
        max_dispatches: 10,
        cost_budget: 3.0, // only 3 cost units
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    // Each candidate costs 1.
    let result = optimizer.select(vec![
        make_candidate("a", "x", 5.0, "aud1"),
        make_candidate("b", "y", 4.0, "aud2"),
        make_candidate("c", "z", 3.0, "aud3"),
        make_candidate("d", "w", 2.0, "aud4"),
    ]);
    // Should select 3 (cost budget = 3, each costs 1).
    assert_eq!(result.selected.len(), 3);
    assert_eq!(result.rejected.len(), 1);
    assert_eq!(result.rejected[0].reason, RejectionReason::BudgetExhausted);
}

#[test]
fn cost_budget_with_variable_costs() {
    let config = PortfolioConfig {
        max_dispatches: 10,
        cost_budget: 5.0,
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let mut expensive = make_candidate("a", "x", 10.0, "aud1");
    expensive.decision_value.resource_cost = ResourceCost::configured(3.0);
    let mut cheap1 = make_candidate("b", "y", 4.0, "aud2");
    cheap1.decision_value.resource_cost = ResourceCost::configured(1.0);
    let mut cheap2 = make_candidate("c", "z", 3.0, "aud3");
    cheap2.decision_value.resource_cost = ResourceCost::configured(1.0);
    let mut cheap3 = make_candidate("d", "w", 2.0, "aud4");
    cheap3.decision_value.resource_cost = ResourceCost::configured(1.0);
    let result = optimizer.select(vec![expensive, cheap1, cheap2, cheap3]);
    // expensive (cost 3) + cheap1 (cost 1) + cheap2 (cost 1) = cost 5.
    // cheap3 would exceed budget.
    assert_eq!(result.selected.len(), 3);
}

#[test]
fn fatigue_reduces_repeated_dispatches() {
    let config = PortfolioConfig {
        max_dispatches: 5,
        cost_budget: 0.0,
        audience_overlap_penalty: 0.0, // disable overlap to isolate fatigue
        fatigue_decay: 0.5,            // 50% reduction per additional dispatch
        min_marginal_value: 1.0,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    // Three candidates to the same audience, all with 10 fans.
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "same"),
        make_candidate("b", "y", 10.0, "same"),
        make_candidate("c", "z", 10.0, "same"),
    ]);
    // First: marginal = 10.0 (no fatigue).
    // Second: marginal = 10.0 * 0.5 = 5.0 (> 1.0, selected).
    // Third: marginal = 10.0 * 0.25 = 2.5 (> 1.0, selected).
    assert_eq!(result.selected.len(), 3);
    // Total = 10.0 + 5.0 + 2.5 = 17.5
    assert!((result.total_expected_fans - 17.5).abs() < 0.01);
}

#[test]
fn fatigue_combined_with_overlap() {
    let config = PortfolioConfig {
        max_dispatches: 5,
        cost_budget: 0.0,
        audience_overlap_penalty: 0.3,
        fatigue_decay: 0.8,
        min_marginal_value: 0.5,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "same"),
        make_candidate("b", "y", 10.0, "same"),
    ]);
    // First: marginal = 10.0 * 1.0 * 1.0 = 10.0
    // Second: marginal = 10.0 * (1 - 0.3) * 0.8 = 10.0 * 0.7 * 0.8 = 5.6
    assert_eq!(result.selected.len(), 2);
    assert!((result.total_expected_fans - 15.6).abs() < 0.01);
}

#[test]
fn cost_budget_zero_uses_max_dispatches_only() {
    let config = PortfolioConfig {
        max_dispatches: 2,
        cost_budget: 0.0, // disabled
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let result = optimizer.select(vec![
        make_candidate("a", "x", 5.0, "aud1"),
        make_candidate("b", "y", 4.0, "aud2"),
        make_candidate("c", "z", 3.0, "aud3"),
    ]);
    // cost_budget=0 means disabled → max_dispatches=2 is the limit.
    assert_eq!(result.selected.len(), 2);
}

// ── T7: EFE cannot hide better DecisionValue ──
//
// The portfolio optimizer ranks by DecisionValue.total() (expected
// incremental Y30 fans), NOT by EFE. EFE decides candidate generation
// (which opportunities enter the pool); DecisionValue decides portfolio
// selection (which candidates win). This test locks the invariant:
// a candidate with materially higher DecisionValue must be selected
// over one with lower DecisionValue, regardless of EFE ordering.

#[test]
fn efe_cannot_hide_better_decision_value() {
    // Candidate A: low EFE (better exploration) but low DecisionValue.
    // Candidate B: high EFE (worse exploration) but high DecisionValue.
    // The portfolio optimizer must select B first because it ranks
    // by DecisionValue.total(), not EFE.
    //
    // We simulate this by giving B a higher expected_fans (which is
    // the primary component of DecisionValue.total()). The EFE score
    // is not passed to the portfolio optimizer at all — it only sees
    // PortfolioCandidate which carries DecisionValue, not EFE.
    let optimizer = PortfolioOptimizer::default();
    let result = optimizer.select(vec![
        make_candidate("a", "x", 2.0, "aud1"),  // low DecisionValue
        make_candidate("b", "y", 10.0, "aud2"), // high DecisionValue
    ]);
    assert!(!result.do_nothing);
    // B must be selected first — higher DecisionValue wins.
    assert_eq!(result.selected[0].opportunity_id.template_id, "b");
    // Both should be selected (different audiences, no overlap).
    assert_eq!(result.selected.len(), 2);
}

#[test]
fn efe_does_not_affect_portfolio_ranking() {
    // Even when the candidate with lower DecisionValue appears first
    // in the input vector (simulating lower EFE = earlier pool entry),
    // the portfolio optimizer must still rank by DecisionValue.
    let optimizer = PortfolioOptimizer::default();
    let candidates = vec![
        make_candidate("low_value", "x", 1.0, "aud1"),
        make_candidate("high_value", "y", 20.0, "aud2"),
        make_candidate("mid_value", "z", 10.0, "aud3"),
    ];
    let result = optimizer.select(candidates);
    // Selection order must be by DecisionValue descending:
    // high_value (20) > mid_value (10) > low_value (1).
    assert_eq!(result.selected[0].opportunity_id.template_id, "high_value");
    assert_eq!(result.selected[1].opportunity_id.template_id, "mid_value");
    assert_eq!(result.selected[2].opportunity_id.template_id, "low_value");
}

// ── P0-2: Experimental exploration budget tests (X1-X6) ──

fn make_experimental_candidate(
    template: &str,
    target: &str,
    expected_fans: f64,
    audience: &str,
) -> PortfolioCandidate {
    let mut c = make_candidate(template, target, expected_fans, audience);
    c.is_experimental = true;
    c
}

/// X1: Normal budget exhausted, experiment budget available →
/// experimental treatment dispatched.
#[test]
fn x1_experimental_slot_used_when_normal_budget_exhausted() {
    let config = PortfolioConfig {
        max_dispatches: 2,
        experimental_dispatch_budget: 1,
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    // 2 normal candidates + 1 experimental candidate.
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "aud1"),
        make_candidate("b", "y", 8.0, "aud2"),
        make_experimental_candidate("exp", "z", 5.0, "aud3"),
    ]);
    // All 3 should be selected: 2 normal + 1 experimental.
    assert_eq!(result.selected.len(), 3);
    assert!(result.selected.iter().any(|c| c.is_experimental));
}

/// X2: Experimental budget exhausted → no additional experimental
/// dispatches.
#[test]
fn x2_experimental_budget_exhausted_rejects_extra() {
    let config = PortfolioConfig {
        max_dispatches: 1,
        experimental_dispatch_budget: 1,
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    // 1 normal + 2 experimental, but only 1 experimental slot.
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "aud1"),
        make_experimental_candidate("exp1", "y", 5.0, "aud2"),
        make_experimental_candidate("exp2", "z", 4.0, "aud3"),
    ]);
    // Only 2 selected: 1 normal + 1 experimental.
    assert_eq!(result.selected.len(), 2);
    // The second experimental candidate should be rejected.
    assert!(
        result
            .rejected
            .iter()
            .any(|r| r.opportunity_key.contains("exp2"))
    );
}

/// X3: Low-value experiment → experimental budget NOT spent merely to
/// increase N. The candidate must still clear min_marginal_value.
#[test]
fn x3_low_value_experiment_not_dispatched() {
    let config = PortfolioConfig {
        max_dispatches: 1,
        experimental_dispatch_budget: 3,
        min_marginal_value: 5.0,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    // Normal candidate with high value, experimental with negligible value.
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "aud1"),
        make_experimental_candidate("exp", "y", 0.02, "aud2"),
    ]);
    // Only the normal candidate is selected.
    assert_eq!(result.selected.len(), 1);
    assert!(!result.selected[0].is_experimental);
}

/// X4: Control assignment → zero experimental treatment slots consumed.
/// (This is tested at the application layer — the optimizer only sees
/// treatment candidates. Control assignments never enter the pool.)
/// Here we verify that non-experimental candidates don't consume
/// experimental slots.
#[test]
fn x4_non_experimental_does_not_consume_experimental_slots() {
    let config = PortfolioConfig {
        max_dispatches: 2,
        experimental_dispatch_budget: 1,
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    // 3 non-experimental candidates, 1 experimental.
    let result = optimizer.select(vec![
        make_candidate("a", "x", 10.0, "aud1"),
        make_candidate("b", "y", 8.0, "aud2"),
        make_candidate("c", "z", 6.0, "aud3"),
        make_experimental_candidate("exp", "w", 5.0, "aud4"),
    ]);
    // 2 normal + 1 experimental = 3 total.
    assert_eq!(result.selected.len(), 3);
    // The 3rd non-experimental candidate should be rejected (normal
    // budget exhausted, and it's not experimental).
    assert!(
        result
            .rejected
            .iter()
            .any(|r| r.opportunity_key.starts_with("c:"))
    );
}

/// X5: Sufficient eligible + budget → minimum capacity reachable.
#[test]
fn x5_sufficient_budget_reaches_capacity() {
    let config = PortfolioConfig {
        max_dispatches: 3,
        experimental_dispatch_budget: 2,
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let result = optimizer.select(vec![
        make_experimental_candidate("exp1", "a", 10.0, "aud1"),
        make_experimental_candidate("exp2", "b", 8.0, "aud2"),
        make_experimental_candidate("exp3", "c", 6.0, "aud3"),
        make_experimental_candidate("exp4", "d", 4.0, "aud4"),
        make_experimental_candidate("exp5", "e", 2.0, "aud5"),
    ]);
    // All 5 should be selected: 3 normal slots + 2 experimental slots.
    assert_eq!(result.selected.len(), 5);
}

/// X6: Safety / operator ceiling wins. When cost_budget is exceeded,
/// experimental candidates don't bypass it.
#[test]
fn x6_cost_budget_constraint_not_bypassed_by_experimental() {
    let config = PortfolioConfig {
        max_dispatches: 10,
        experimental_dispatch_budget: 5,
        cost_budget: 5.0,
        min_marginal_value: 0.01,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer::new(config);
    let mut expensive_exp = make_experimental_candidate("exp", "x", 10.0, "aud1");
    expensive_exp.decision_value.resource_cost = ResourceCost::configured(10.0);
    let result = optimizer.select(vec![expensive_exp]);
    // The experimental candidate exceeds the cost budget → rejected.
    assert!(result.do_nothing);
    assert!(result.selected.is_empty());
}

// ── PREF-3: Portfolio order-independence ──
//
// The portfolio optimizer must produce the same economic winner
// regardless of input candidate ordering. This is critical because
// tenant preference and EFE both affect candidate ordering before
// the optimizer — if the optimizer were order-dependent, those
// signals would implicitly influence economic selection.

#[test]
fn portfolio_selection_is_order_independent() {
    let optimizer = PortfolioOptimizer::default();
    let candidates = vec![
        make_candidate("a", "x", 5.0, "aud1"),
        make_candidate("b", "y", 10.0, "aud2"),
        make_candidate("c", "z", 3.0, "aud3"),
        make_candidate("d", "w", 7.0, "aud1"), // overlaps with "a"
        make_candidate("e", "v", 5.0, "aud4"), // ties with "a" in total
    ];
    let baseline = optimizer.select(candidates.clone());
    let baseline_ids: HashSet<String> = baseline
        .selected
        .iter()
        .map(|c| c.opportunity_id.template_id.clone())
        .collect();
    let baseline_fans = baseline.total_expected_fans;

    // Run 100 deterministic shuffles
    for seed in 0..100u64 {
        let mut shuffled = candidates.clone();
        // Simple LCG shuffle (deterministic, no external deps)
        let mut state = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        for i in (1..shuffled.len()).rev() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = (state >> 33) as usize % (i + 1);
            shuffled.swap(i, j);
        }
        let result = optimizer.select(shuffled);
        let result_ids: HashSet<String> = result
            .selected
            .iter()
            .map(|c| c.opportunity_id.template_id.clone())
            .collect();
        assert_eq!(
            baseline_ids, result_ids,
            "shuffle seed {seed} produced different selected set"
        );
        assert!(
            (result.total_expected_fans - baseline_fans).abs() < 0.01,
            "shuffle seed {seed} produced different total_expected_fans: {} vs {}",
            result.total_expected_fans,
            baseline_fans
        );
    }
}

#[test]
fn portfolio_ties_in_decision_value_are_order_independent() {
    // Two candidates with identical DecisionValue and different audiences.
    // Both should be selected regardless of input order.
    let optimizer = PortfolioOptimizer::default();
    let a = make_candidate("a", "x", 5.0, "aud1");
    let b = make_candidate("b", "y", 5.0, "aud2");
    let r1 = optimizer.select(vec![a.clone(), b.clone()]);
    let r2 = optimizer.select(vec![b, a]);
    assert_eq!(r1.selected.len(), r2.selected.len());
    assert!(
        (r1.total_expected_fans - r2.total_expected_fans).abs() < 0.01,
        "tied candidates should produce same total regardless of order"
    );
}
