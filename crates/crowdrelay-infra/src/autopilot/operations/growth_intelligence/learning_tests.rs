//! Learning-loop invariants for `apply_evidence_to_model`.
//!
//! Split out of the parent so the loader stays inside the source-size ratchet.
//! These are unit tests over deterministic in-memory models — no database, no
//! clock, no network — because the questions they ask are about arithmetic and
//! attribution, not persistence: does an outcome move what it should, does it
//! leave alone what it should, and does what it moved reach the number the
//! optimizer ranks on.

use super::apply_evidence_to_model;
use crowdrelay_brain::{CausalModel, DispatchContext, GrowthEvidence, TreatmentAssignment};

/// Brain-level evidence-eligibility invariant:
///
/// Evidence with a treatment assignment but NO observed outcome
/// (`observed_incremental_fans = None`) must NOT move the
/// treatment-effect posterior. The `apply_evidence_to_model`
/// function guards `update_treatment_effect` behind
/// `if let Some(tau_y14) = ev.observed_incremental_fans`, so
/// absent outcomes are naturally skipped. This test proves the
/// guard works by constructing real evidence and passing it
/// through the actual evidence-processing path.
///
/// This is the brain-level complement to T25i (which proves the
/// SQL boundary excludes UNKNOWN evidence). Together they form
/// two independent defenses:
/// - T25i → SQL/persistence learning-boundary proof
/// - This test → model-level evidence-eligibility proof
#[test]
fn evidence_without_observed_outcome_does_not_update_treatment_posterior() {
    let mut model = CausalModel::new();
    let ctx = DispatchContext::default();
    let template = "community.engage";
    let before = model.predict_stats_with_treatment(template, &ctx);

    // Construct real evidence with treatment assignment but no
    // observed outcome. This is what an unresolved/UNKNOWN dispatch
    // looks like if it somehow reached the learner.
    let evidence = GrowthEvidence {
        opportunity_id: Some(format!("{template}:subreddit:community.engage.request:ctx")),
        treatment: TreatmentAssignment::Treatment,
        observed_incremental_fans: None, // ← no outcome
        observed_fans: None,             // ← no raw outcome either
        ..GrowthEvidence::default()
    };

    // Pass through the real evidence-processing path.
    apply_evidence_to_model(&mut model, &[evidence]);

    let after = model.predict_stats_with_treatment(template, &ctx);
    assert_eq!(
        before.treatment_effect, after.treatment_effect,
        "treatment posterior must not move when observed outcome is absent"
    );
    assert_eq!(
        before.treatment_confidence, after.treatment_confidence,
        "treatment confidence must not change when observed outcome is absent"
    );
    assert_eq!(
        before.use_treatment_effect, after.use_treatment_effect,
        "treatment activation must not change when observed outcome is absent"
    );
}

/// Positive control: evidence WITH an observed outcome DOES move
/// the treatment-effect posterior. This proves the evidence path
/// is actually exercised — without this test, the negative test
/// above could be vacuously true because `apply_evidence_to_model`
/// does nothing at all.
#[test]
fn evidence_with_observed_outcome_updates_treatment_posterior() {
    let mut model = CausalModel::new();
    let ctx = DispatchContext::default();
    let template = "community.engage";
    let before = model.predict_stats_with_treatment(template, &ctx);

    // Construct evidence with a real observed outcome.
    let evidence = GrowthEvidence {
        opportunity_id: Some(format!("{template}:subreddit:community.engage.request:ctx")),
        treatment: TreatmentAssignment::Treatment,
        observed_incremental_fans: Some(5.0), // ← real outcome
        observed_fans: Some(10.0),            // ← raw outcome
        predicted_fans: 3.0,
        ..GrowthEvidence::default()
    };

    apply_evidence_to_model(&mut model, &[evidence]);

    let after = model.predict_stats_with_treatment(template, &ctx);
    // The treatment effect estimate should have moved from the
    // prior (0.0) toward the observed value (5.0).
    assert_ne!(
        before.treatment_effect, after.treatment_effect,
        "treatment posterior must move when an observed outcome is present"
    );
}

/// One resolved Y14 outcome moves the Y14 posterior and leaves Y30 alone.
///
/// This is the same distinction the seven-day measurement got wrong at the
/// storage layer, asserted one level up: an outcome speaks for the horizon
/// it measured and for no other. A Y14 observation that nudged Y30 would
/// hand the North Star posterior confidence it never earned, and the number
/// it produced would look entirely reasonable while doing it.
#[test]
fn a_resolved_y14_outcome_moves_y14_and_not_y30() {
    let mut model = CausalModel::new();
    let ctx = DispatchContext::default();
    let template = "community-engager";
    let target = "community:aaaaaaaa-0000-0000-0000-000000000001";

    let before = model.predict_stats_with_treatment_for_target(template, Some(target), &ctx);
    assert_eq!(
        before.treatment_confidence, 0,
        "the fixture must start from an untaught posterior"
    );

    apply_evidence_to_model(
        &mut model,
        &[GrowthEvidence {
            opportunity_id: Some(format!("{template}:target:post:ctx")),
            target_key: Some(target.to_owned()),
            treatment: TreatmentAssignment::Treatment,
            observed_incremental_fans: Some(6.0),
            observed_fans: Some(9.0),
            predicted_fans: 2.0,
            ..GrowthEvidence::default()
        }],
    );

    let after = model.predict_stats_with_treatment_for_target(template, Some(target), &ctx);

    assert!(
        after.treatment_effect > before.treatment_effect,
        "a positive Y14 outcome must move the Y14 estimate upward, {} to {}",
        before.treatment_effect,
        after.treatment_effect
    );
    // The fixture's evidence is `Observational`, so it moves the estimate
    // without buying identification. Confidence is quality-weighted: one
    // observational row is a tenth of an observation, and the regime switch
    // stays where it was.
    assert_eq!(
        after.treatment_confidence, 0,
        "an observational row must not buy treatment confidence"
    );
    assert!(
        after.expected_fans > before.expected_fans,
        "the outcome model must see the raw count, {} to {}",
        before.expected_fans,
        after.expected_fans
    );

    // The horizon that was not measured.
    assert_eq!(
        after.treatment_effect_y30, before.treatment_effect_y30,
        "a Y14 outcome must not move the Y30 estimate"
    );
    assert_eq!(
        after.treatment_confidence_y30, 0,
        "nor may Y30 gain confidence from an observation it never saw"
    );
    assert!(
        !after.uses_y30,
        "and Y30 must not become the ranking signal on Y14 evidence alone"
    );
    assert_eq!(
        after.bridge_confidence, before.bridge_confidence,
        "the Y14-to-Y30 bridge needs both outcomes and saw only one"
    );
}

/// What the model learned reaches the number the optimizer ranks on.
///
/// A moving posterior is necessary and not sufficient: the value the
/// portfolio sorts by is `DecisionValue::total()`, and a posterior that
/// never reached it would leave the brain storing experience rather than
/// using it. Two candidates are scored from identical starting models, one
/// of which is then shown the outcome, so the difference between them is
/// attributable to the evidence and to nothing else.
///
/// This asserts the last hop only. It does not require the ranking to
/// invert — that depends on how large the made-up outcome happens to be —
/// and it deliberately does not assert that an unrelated template stays
/// put. The outcome model pools toward a shared root, so one observation
/// moves every template's expectation; that is partial pooling working as
/// designed, and a test claiming otherwise would be describing a different
/// model.
#[test]
fn a_learned_posterior_reaches_the_decision_value() {
    use crowdrelay_brain::{DecisionMode, DecisionValue, ResourceCost};

    let ctx = DispatchContext::default();
    let template = "community-engager";
    let target = "community:aaaaaaaa-0000-0000-0000-000000000001";
    let cost = ResourceCost::default();

    let untaught = CausalModel::new();
    let mut taught = CausalModel::new();

    let value_of = |model: &CausalModel| {
        DecisionValue::from_stats(
            &model.predict_stats_with_treatment_for_target(template, Some(target), &ctx),
            cost,
            DecisionMode::Exploit,
        )
    };

    let before = value_of(&untaught);
    assert_eq!(
        before.total(),
        value_of(&taught).total(),
        "two candidates from identical models must score identically"
    );

    apply_evidence_to_model(
        &mut taught,
        &[GrowthEvidence {
            opportunity_id: Some(format!("{template}:target:post:ctx")),
            target_key: Some(target.to_owned()),
            treatment: TreatmentAssignment::Treatment,
            observed_incremental_fans: Some(6.0),
            observed_fans: Some(9.0),
            predicted_fans: 2.0,
            ..GrowthEvidence::default()
        }],
    );

    let after = value_of(&taught);

    assert_ne!(
        before.total(),
        after.total(),
        "a resolved outcome must change the value the optimizer ranks on"
    );
    assert!(
        after.total() > before.total(),
        "and a candidate that produced more fans than predicted must score higher, {} to {}",
        before.total(),
        after.total()
    );
    assert_eq!(
        before.total(),
        value_of(&untaught).total(),
        "scoring must not mutate the model it reads"
    );
}
