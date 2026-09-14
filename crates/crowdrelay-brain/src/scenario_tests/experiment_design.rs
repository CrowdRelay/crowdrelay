//! Experiment design scenarios (M–S) of the behavioural evaluation harness.
//!
//! Split out of the parent so `scenario_tests.rs` stays inside the source-size
//! ratchet. A clean cut: this group shares none of the parent's candidate and
//! decision-value helpers, builds its own `ExperimentDesign` fixtures, and
//! exercises a different part of the brain — the causal structure of an
//! experiment rather than which action a portfolio picks.
//!
//! What these prove, per the parent's harness contract: the same experiment
//! carries both arms, rounds keep one outcome unit, experiments stay isolated
//! from each other, a workspace outcome cannot masquerade as a community one,
//! contamination discovered late still downgrades the evidence, unattributed
//! credit stays unattributed, and calibration regimes do not bleed into
//! each other.

use crate::calibration::CalibrationByRegime;
use crate::credit_ledger::{
    ActionExposure, CreditAllocator, FanOutcome, ProportionalCreditAllocator,
};
use crate::decision_value::EstimationRegime;
use crate::evidence::EvidenceQuality;
use crate::experiment::{
    ExperimentAssignment, ExperimentDesign, ExperimentUnitKind, InterferencePolicy,
    TreatmentAssignment,
};

fn make_exp_prediction(template: &str) -> crate::causal_model::DispatchPrediction {
    crate::causal_model::DispatchPrediction {
        template_id: template.to_owned(),
        expected_new_fans: 5.0,
        expected_signal_installs: 1.0,
        context: crate::causal_model::DispatchContext::default(),
        target_key: None,
        creative_family: None,
    }
}

fn make_exp_design(units: &[&str]) -> ExperimentDesign {
    let uuid = uuid::Uuid::now_v7();
    ExperimentDesign::new(
        uuid,
        "community.engage",
        "cycle-test",
        ExperimentUnitKind::TargetCommunity,
        units.iter().map(|s| s.to_string()).collect(),
        time::OffsetDateTime::now_utc(),
        0.05,
        "discovery",
    )
}

// ── M: Same experiment, both arms ──
//
// Verify that treatment and control assignments for different units
// share the same experiment_uuid and assignment_round. The estimator
// can pair them because they belong to the same experiment universe.

#[test]
fn m_same_experiment_both_arms() {
    let design = make_exp_design(&["r/djent", "r/metalcore", "r/progmetal"]);
    let pred = make_exp_prediction("community.engage");

    let treatment = ExperimentAssignment::from_design(
        &design,
        "r/djent",
        "r/djent",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );
    let control = ExperimentAssignment::from_design(
        &design,
        "r/metalcore",
        "r/metalcore",
        TreatmentAssignment::Control,
        &pred,
        None,
    );
    let treatment2 = ExperimentAssignment::from_design(
        &design,
        "r/progmetal",
        "r/progmetal",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );

    // All assignments share the same experiment_uuid.
    assert_eq!(treatment.experiment_uuid, design.experiment_uuid);
    assert_eq!(control.experiment_uuid, design.experiment_uuid);
    assert_eq!(treatment2.experiment_uuid, design.experiment_uuid);

    // All share the same assignment_round.
    assert_eq!(treatment.assignment_round, control.assignment_round);
    assert_eq!(treatment.assignment_round, treatment2.assignment_round);

    // Different units.
    assert_ne!(treatment.unit_id, control.unit_id);
    assert_ne!(treatment.unit_id, treatment2.unit_id);

    // Different arms: treatment and control coexist in the same experiment.
    assert_eq!(treatment.arm, TreatmentAssignment::Treatment);
    assert_eq!(control.arm, TreatmentAssignment::Control);
    assert_eq!(treatment2.arm, TreatmentAssignment::Treatment);

    // The estimator can pair treatment and control because they share
    // the experiment_uuid. Treatment has action_id, control does not.
    assert!(treatment.action_id.is_some());
    assert!(control.action_id.is_none());
    assert!(treatment2.action_id.is_some());

    // Propensity is arm-specific: treatment gets 1 - holdout, control
    // gets holdout.
    assert!((treatment.propensity - (1.0 - 0.05)).abs() < 1e-10);
    assert!((control.propensity - 0.05).abs() < 1e-10);
}

// ── N: Same unit across rounds ──
//
// Verify that the same unit can appear in different experiments (different
// experiment_uuid) without collision. Each cycle creates a new experiment.

#[test]
fn n_same_unit_across_experiments() {
    let design1 = make_exp_design(&["r/djent"]);
    let design2 = make_exp_design(&["r/djent"]);
    let pred = make_exp_prediction("community.engage");

    let a1 = ExperimentAssignment::from_design(
        &design1,
        "r/djent",
        "r/djent",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );
    let a2 = ExperimentAssignment::from_design(
        &design2,
        "r/djent",
        "r/djent",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );

    // Different experiment_uuids — no collision.
    assert_ne!(a1.experiment_uuid, a2.experiment_uuid);

    // Same unit_id, same round — but different experiments.
    assert_eq!(a1.unit_id, a2.unit_id);
    assert_eq!(a1.assignment_round, a2.assignment_round);

    // Different assignment_ids.
    assert_ne!(a1.assignment_id, a2.assignment_id);

    // Both are valid assignments — the unique constraint is on
    // (experiment_uuid, assignment_round, unit_id), so these don't
    // collide because the experiment_uuids are different.
}

// ── O: Cross-experiment isolation ──
//
// Two unrelated experiments (different interventions) on the same workspace
// must not contaminate each other's assignments. The experiment_uuid
// uniquely identifies each experiment, and contamination evaluation
// only counts concurrent actions from the SAME unit in DIFFERENT
// experiments.

#[test]
fn o_cross_experiment_isolation() {
    let design_community = ExperimentDesign::new(
        uuid::Uuid::now_v7(),
        "community.engage",
        "cycle-o",
        ExperimentUnitKind::TargetCommunity,
        vec!["r/djent".to_owned()],
        time::OffsetDateTime::now_utc(),
        0.05,
        "discovery",
    );
    let design_scanner = ExperimentDesign::new(
        uuid::Uuid::now_v7(),
        "scanner.discover",
        "cycle-o",
        ExperimentUnitKind::TargetCommunity,
        vec!["r/djent".to_owned()],
        time::OffsetDateTime::now_utc(),
        0.05,
        "discovery",
    );
    let pred = make_exp_prediction("community.engage");

    let community_assignment = ExperimentAssignment::from_design(
        &design_community,
        "r/djent",
        "r/djent",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );
    let scanner_assignment = ExperimentAssignment::from_design(
        &design_scanner,
        "r/djent",
        "r/djent",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );

    // Different experiment_uuids — isolated experiments.
    assert_ne!(
        community_assignment.experiment_uuid,
        scanner_assignment.experiment_uuid
    );

    // Different interference policies — community.engage is potentially
    // isolatable, scanner.discover is maybe not isolatable.
    assert_eq!(
        community_assignment.interference_policy,
        InterferencePolicy::PotentiallyIsolatable
    );
    assert_eq!(
        scanner_assignment.interference_policy,
        InterferencePolicy::MaybeNotIsolatable
    );

    // Different intended_template_ids — different interventions.
    assert_ne!(
        community_assignment.intended_template_id,
        scanner_assignment.intended_template_id
    );
}

// ── P: Workspace outcome cannot masquerade as community outcome ──
//
// Growth intelligence dispatches are workspace-level agent runs. The
// experiment unit is Workspace, not TargetCommunity. This means:
// - Interference policy is NotIsolatable (workspace actions spill)
// - Experiment kind is MatchedQuasiExperiment (no clean holdout)
// - Evidence quality is matched_quasi_experiment, NOT randomized_holdout
//
// This test verifies that the unit_kind → interference_policy →
// experiment_kind chain is correct for both Workspace and
// TargetCommunity unit kinds, and that the evidence quality enum
// values match what the measurement layer expects.

#[test]
fn p_workspace_outcome_cannot_masquerade_as_community() {
    // Workspace unit (growth intelligence dispatch): NotIsolatable →
    // MatchedQuasiExperiment. This is the honest classification —
    // workspace-level agent runs can't be isolated from interference.
    let ws_design = ExperimentDesign::new(
        uuid::Uuid::now_v7(),
        "community-engager",
        "cycle-p",
        ExperimentUnitKind::Workspace,
        vec!["decision:growth-intelligence:v1:community-engager:1".to_owned()],
        time::OffsetDateTime::now_utc(),
        0.05,
        "discovery",
    );
    assert_eq!(
        ws_design.interference_policy,
        InterferencePolicy::NotIsolatable
    );
    let pred = make_exp_prediction("community-engager");
    let ws_assignment = ExperimentAssignment::from_design(
        &ws_design,
        "decision:growth-intelligence:v1:community-engager:1",
        "decision:growth-intelligence:v1:community-engager:1",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );
    assert_eq!(
        ws_assignment.kind(),
        crate::experiment::ExperimentKind::MatchedQuasiExperiment
    );
    assert!(!ws_assignment.is_interference_controllable);

    // TargetCommunity unit with community.engage (dot-style):
    // PotentiallyIsolatable → RandomizedHoldout. This is the ideal
    // case for future community-targeted experiments.
    let tc_design = make_exp_design(&["r/djent"]);
    assert_eq!(
        tc_design.interference_policy,
        InterferencePolicy::PotentiallyIsolatable
    );
    let tc_assignment = ExperimentAssignment::from_design(
        &tc_design,
        "r/djent",
        "r/djent",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );
    assert_eq!(
        tc_assignment.kind(),
        crate::experiment::ExperimentKind::RandomizedHoldout
    );
    assert!(tc_assignment.is_interference_controllable);

    // TargetCommunity with kebab-case template ID (community-engager):
    // also PotentiallyIsolatable (bug #1 fix).
    let kebab_design = ExperimentDesign::new(
        uuid::Uuid::now_v7(),
        "community-engager",
        "cycle-kebab",
        ExperimentUnitKind::TargetCommunity,
        vec!["r/djent".to_owned()],
        time::OffsetDateTime::now_utc(),
        0.05,
        "discovery",
    );
    assert_eq!(
        kebab_design.interference_policy,
        InterferencePolicy::PotentiallyIsolatable
    );

    // TargetCommunity with global-* template: NotIsolatable.
    let global_design = ExperimentDesign::new(
        uuid::Uuid::now_v7(),
        "global-blast",
        "cycle-global",
        ExperimentUnitKind::TargetCommunity,
        vec!["r/djent".to_owned()],
        time::OffsetDateTime::now_utc(),
        0.05,
        "discovery",
    );
    assert_eq!(
        global_design.interference_policy,
        InterferencePolicy::NotIsolatable
    );

    // The evidence quality enum values must match what the measurement
    // layer writes to the DB. The measurement code uses string literals
    // "randomized_holdout" and "matched_quasi_experiment".
    assert_eq!(
        EvidenceQuality::RandomizedHoldout.as_str(),
        "randomized_holdout"
    );
    assert_eq!(
        EvidenceQuality::MatchedQuasiExperiment.as_str(),
        "matched_quasi_experiment"
    );
}

// ── Q: Contamination discovered later ──
//
// At assignment time, the unit is clean (contamination = 0). Later,
// a concurrent treatment action occurs on the same unit. The
// evaluate_contamination function (in the infra layer) detects this
// and downgrades final_evidence_quality. This test verifies the
// assignment-time state and the contamination → evidence quality
// downgrade logic at the type level.

#[test]
fn q_contamination_discovered_later() {
    let design = make_exp_design(&["r/djent"]);
    let pred = make_exp_prediction("community.engage");

    let assignment = ExperimentAssignment::from_design(
        &design,
        "r/djent",
        "r/djent",
        TreatmentAssignment::Treatment,
        &pred,
        Some(uuid::Uuid::now_v7()),
    );

    // At assignment time, interference_score is 0 (clean).
    assert!((assignment.interference_score - 0.0).abs() < 1e-10);

    // The interference policy is PotentiallyIsolatable (community.engage
    // on TargetCommunity). The final contamination scan (done by the
    // measurement layer over the full window) can upgrade this to
    // contaminated if concurrent treatment actions occur.
    assert!(assignment.is_interference_controllable);

    // The experiment_kind at assignment time is RandomizedHoldout.
    // The final_evidence_quality (set by evaluate_contamination in the
    // infra layer) can downgrade this to MatchedQuasiExperiment.
    assert_eq!(
        assignment.kind(),
        crate::experiment::ExperimentKind::RandomizedHoldout
    );

    // Verify the evidence quality downgrade path: when contamination
    // is high, the evidence quality must be downgraded. The infra
    // layer's evaluate_contamination uses a threshold of 0.1. We
    // verify here that the EvidenceQuality enum supports the
    // downgrade target and that the weight decreases (weaker
    // evidence gets less influence on the posterior).
    let clean_quality = EvidenceQuality::RandomizedHoldout;
    let contaminated_quality = EvidenceQuality::MatchedQuasiExperiment;

    // Contaminated evidence must have a lower weight (less influence
    // on the posterior) than clean evidence.
    assert!(
        contaminated_quality.weight() < clean_quality.weight(),
        "contaminated evidence should have lower weight"
    );

    // And a higher variance multiplier (trusted less).
    assert!(
        contaminated_quality.variance_multiplier() > clean_quality.variance_multiplier(),
        "contaminated evidence should have higher variance multiplier"
    );
}

// ── R: Unattributed credit ──
//
// Low-confidence attribution must leave a genuine residual. The
// ProportionalCreditAllocator scales total attribution mass by mean
// confidence, so low confidence → large residual.

#[test]
fn r_unattributed_credit_survives() {
    use crate::evidence::EvidenceQuality;

    let allocator = ProportionalCreditAllocator;
    let outcome = FanOutcome {
        workspace_id: uuid::Uuid::nil(),
        observed_incremental_fans: 10.0,
        durable_fans_30d: None,
        measurement_window_start: time::OffsetDateTime::now_utc(),
        measurement_window_end: time::OffsetDateTime::now_utc(),
    };

    // Low confidence (0.3) → 30% attributed, 70% residual.
    let actions = [ActionExposure {
        action_id: uuid::Uuid::now_v7(),
        template_id: "community.engage".to_string(),
        audience_key: "r/djent".to_string(),
        exposure_delivered: true,
        temporal_proximity: 1.0,
        audience_match: 1.0,
        attribution_confidence: 0.3,
        treatment_effect_prior: 0.0,
        evidence_quality: EvidenceQuality::Observational,
    }];
    let result = allocator.allocate(&outcome, &actions);
    assert_eq!(result.credits.len(), 1);
    assert!(
        (result.credits[0].credited_incremental_y14 - 3.0).abs() < 0.001,
        "credited = {}",
        result.credits[0].credited_incremental_y14
    );
    assert!(
        (result.unattributed - 7.0).abs() < 0.001,
        "unattributed = {}",
        result.unattributed
    );

    // Full confidence (1.0) → 100% attributed, 0 residual.
    let actions_full = [ActionExposure {
        action_id: uuid::Uuid::now_v7(),
        template_id: "community.engage".to_string(),
        audience_key: "r/djent".to_string(),
        exposure_delivered: true,
        temporal_proximity: 1.0,
        audience_match: 1.0,
        attribution_confidence: 1.0,
        treatment_effect_prior: 0.0,
        evidence_quality: EvidenceQuality::Observational,
    }];
    let result_full = allocator.allocate(&outcome, &actions_full);
    assert!((result_full.credits[0].credited_incremental_y14 - 10.0).abs() < 0.001);
    assert!(result_full.unattributed < 0.001);

    // No actions → all unattributed.
    let result_none = allocator.allocate(&outcome, &[]);
    assert!(result_none.credits.is_empty());
    assert!((result_none.unattributed - 10.0).abs() < 0.001);
}

// ── S: Calibration regime isolation ──
//
// A terrible OutcomeModel calibration (slope=0.1) must NOT distort
// Y30Direct treatment uncertainty. Record bad OutcomeModel predictions,
// good Y30Direct predictions. Verify correct_uncertainty_by_regime
// is unaffected by the bad OutcomeModel calibration.

#[test]
fn s_calibration_regime_isolation() {
    let mut cal = CalibrationByRegime::new();

    // Record 10 bad OutcomeModel predictions: predicted varies, observed
    // is always much lower. This gives a very low calibration slope (<< 1.0),
    // meaning the predictor is over-confident and uncertainty should be
    // inflated.
    for i in 0..10 {
        let predicted = 5.0 + (i as f64) * 2.0; // 5, 7, 9, 11, ... 23
        let observed = 0.5 + (i as f64) * 0.1; // 0.5, 0.6, 0.7, ... 1.4 (much lower)
        cal.record_by_regime(
            EstimationRegime::OutcomeModel,
            "community.engage",
            predicted,
            2.0,
            observed,
            Some("r/djent"),
            None,
            "observational",
        );
    }

    // Record 10 good Y30Direct predictions: predicted ≈ observed.
    // This gives a calibration slope of ~1.0 (well-calibrated).
    for i in 0..10 {
        let predicted = 5.0 + (i as f64) * 1.0; // 5, 6, 7, ... 14
        let observed = predicted; // perfect calibration
        cal.record_by_regime(
            EstimationRegime::Y30Direct,
            "community.engage",
            predicted,
            2.0,
            observed,
            Some("r/djent"),
            None,
            "randomized_holdout",
        );
    }

    // The OutcomeModel calibration should inflate uncertainty heavily
    // (slope << 1.0 → over-confident → inflate).
    let std_outcome = cal.correct_uncertainty_by_regime(EstimationRegime::OutcomeModel, 2.0);
    assert!(
        std_outcome > 2.0,
        "OutcomeModel should inflate uncertainty (slope < 1), got {std_outcome}"
    );

    // The Y30Direct calibration should NOT be distorted by the bad
    // OutcomeModel calibration. With slope ≈ 1.0, the correction
    // should be minimal.
    let std_y30 = cal.correct_uncertainty_by_regime(EstimationRegime::Y30Direct, 2.0);
    assert!(
        (std_y30 - 2.0).abs() < 0.5,
        "Y30Direct should be well-calibrated (slope ≈ 1), got {std_y30}"
    );

    // The two regimes produce different corrections — they are isolated.
    assert!(
        (std_outcome - std_y30).abs() > 0.5,
        "Regime isolation: OutcomeModel and Y30Direct must produce different corrections"
    );

    // Y14Bridged should be unaffected by both (no data recorded for it).
    let std_y14 = cal.correct_uncertainty_by_regime(EstimationRegime::Y14Bridged, 2.0);
    assert!(
        (std_y14 - 2.0).abs() < 0.001,
        "Y14Bridged has no data → no correction, got {std_y14}"
    );
}
