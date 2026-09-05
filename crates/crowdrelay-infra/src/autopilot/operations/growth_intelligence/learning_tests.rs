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

/// A control arm from an earlier batch is still this row's counterfactual.
///
/// The learning cursor is `resolved_at` and the randomisation does not respect
/// it. An experiment's treated units resolve when their own measurements
/// finish, which is not the same day for all of them; the control arm resolves
/// once, alongside the first. Every treated row after that used to arrive in a
/// batch containing no control row, be capped at quasi-experimental, and
/// contribute a raw pre/post difference — a randomised experiment degrading to
/// an observational one because of when a checkpoint happened to be taken.
///
/// Two properties, and they pull in opposite directions, which is why both are
/// asserted. The contrast must count: the Y30 treatment effect must be the same
/// whether the control row arrives in the batch or beside it. And the contrast
/// must not be *learned from* twice: the outcome model updates from every row
/// in the learning batch, so a control row supplied as contrast must leave it
/// exactly where it was.
#[test]
fn a_control_arm_from_an_earlier_batch_still_contrasts_and_is_not_relearned() {
    use super::evidence_replay::apply_evidence_to_model_with_contrast;
    use crowdrelay_brain::EvidenceQuality;

    let template = "community-engager";
    let target = "community:aaaaaaaa-0000-0000-0000-000000000004";
    let experiment = uuid::Uuid::from_u128(0x5eed_0003);
    let ctx = DispatchContext::default();

    let treated = GrowthEvidence {
        opportunity_id: Some(format!("{template}:target:post:ctx")),
        target_key: Some(target.to_owned()),
        treatment: TreatmentAssignment::Treatment,
        experiment_uuid: Some(experiment),
        experiment_assignment_id: Some("assignment-treated".to_owned()),
        evidence_quality: EvidenceQuality::RandomizedHoldout,
        final_contamination: Some(0.0),
        observed_incremental_fans: Some(4.0),
        observed_fans: Some(4.0),
        durable_fans_30d: Some(10.0),
        predicted_fans: 2.0,
        ..GrowthEvidence::default()
    };
    let control = GrowthEvidence {
        opportunity_id: Some(format!("{template}:target:post:ctx")),
        target_key: Some(target.to_owned()),
        treatment: TreatmentAssignment::Control,
        experiment_uuid: Some(experiment),
        experiment_assignment_id: Some("assignment-control".to_owned()),
        evidence_quality: EvidenceQuality::RandomizedHoldout,
        final_contamination: Some(0.0),
        observed_incremental_fans: Some(1.0),
        observed_fans: Some(1.0),
        durable_fans_30d: Some(2.0),
        action_id: None,
        ..GrowthEvidence::default()
    };

    let stats_after = |batch: &[GrowthEvidence], contrast: &[GrowthEvidence]| {
        let mut model = CausalModel::new();
        apply_evidence_to_model_with_contrast(&mut model, batch, contrast);
        model.predict_stats_with_treatment_for_target(template, Some(target), &ctx)
    };

    let same_batch = stats_after(&[control.clone(), treated.clone()], &[]);
    let earlier_batch = stats_after(
        std::slice::from_ref(&treated),
        std::slice::from_ref(&control),
    );
    let no_contrast_at_all = stats_after(std::slice::from_ref(&treated), &[]);

    assert!(
        (same_batch.treatment_effect_y30 - earlier_batch.treatment_effect_y30).abs() < 1e-9,
        "a control arm supplied as contrast must produce the same Y30 effect as \
         one in the batch: {} vs {}",
        same_batch.treatment_effect_y30,
        earlier_batch.treatment_effect_y30
    );
    // Whether the contrast is *subtracted* has to be asked at a fixed weight.
    // Comparing against "no contrast at all" cannot answer it: that row is also
    // capped at quasi-experimental, so it moves the posterior less, and the
    // larger tau and the smaller weight pull in opposite directions. Hold the
    // weight and vary only the amount subtracted.
    let contrast_of = |y30: f64| {
        stats_after(
            std::slice::from_ref(&treated),
            &[GrowthEvidence {
                durable_fans_30d: Some(y30),
                ..control.clone()
            }],
        )
        .treatment_effect_y30
    };
    assert!(
        contrast_of(2.0) < contrast_of(0.0),
        "a control arm that itself gained fans must reduce the treated row's \
         attributed effect: contrast 2.0 gives {}, contrast 0.0 gives {}",
        contrast_of(2.0),
        contrast_of(0.0)
    );
    assert!(
        no_contrast_at_all.treatment_effect_y30 > 0.0,
        "the uncontrasted fixture must still teach something, otherwise the \
         comparison above proves nothing"
    );

    // The other direction. The control row's own raw outcome taught the outcome
    // model when it first resolved; handing it back as contrast must not teach
    // it again.
    assert!(
        (earlier_batch.expected_fans - no_contrast_at_all.expected_fans).abs() < 1e-9,
        "a contrast row must not update the outcome model: {} vs {}",
        earlier_batch.expected_fans,
        no_contrast_at_all.expected_fans
    );
    assert!(
        (same_batch.expected_fans - earlier_batch.expected_fans).abs() > 1e-9,
        "the fixture must be able to tell the two apart — a control row in the \
         learning batch does update the outcome model"
    );

    // A row present in both slices is one observation, not two.
    let counted_once = stats_after(
        &[control.clone(), treated.clone()],
        std::slice::from_ref(&control),
    );
    assert!(
        (counted_once.treatment_effect_y30 - same_batch.treatment_effect_y30).abs() < 1e-9,
        "a control row in both slices must be averaged in once: {} vs {}",
        counted_once.treatment_effect_y30,
        same_batch.treatment_effect_y30
    );
}

/// A randomised contrast is earned per horizon, not per experiment.
///
/// Y14 and Y30 close sixteen days apart, so for most of an experiment's life
/// the control arm is resolved on one horizon and pending on the other.
/// `ControlMean` carries an `Option` per horizon to say so. The learner used to
/// read only "does this experiment have a control arm at all", give the treated
/// row full `RandomizedHoldout` weight on both horizons, and then subtract
/// `0.0` for the horizon that had no mean — a raw pre/post difference, weighted
/// as a randomised contrast.
///
/// Two runs with the **same** τ isolate the weighting from the arithmetic: the
/// control's Y30 mean is `0.0` in one and absent in the other, so `y30_fans` is
/// 10.0 either way and only the earned quality differs. A row that cannot name
/// its counterfactual must move the Y30 posterior strictly less than one that
/// can. Under the old code both runs earned `RandomizedHoldout` and the two
/// posteriors landed on the same number.
#[test]
fn a_missing_control_mean_on_one_horizon_does_not_buy_randomised_weight() {
    use crowdrelay_brain::EvidenceQuality;

    let template = "community-engager";
    let target = "community:aaaaaaaa-0000-0000-0000-000000000002";
    let experiment = uuid::Uuid::from_u128(0x5eed_0001);
    let ctx = DispatchContext::default();

    let treated = GrowthEvidence {
        opportunity_id: Some(format!("{template}:target:post:ctx")),
        target_key: Some(target.to_owned()),
        treatment: TreatmentAssignment::Treatment,
        experiment_uuid: Some(experiment),
        // Randomised by design and shown clean, so `effective_evidence_quality`
        // does not cap it before the contrast question is even asked.
        evidence_quality: EvidenceQuality::RandomizedHoldout,
        final_contamination: Some(0.0),
        observed_incremental_fans: Some(4.0),
        observed_fans: Some(4.0),
        durable_fans_30d: Some(10.0),
        predicted_fans: 2.0,
        ..GrowthEvidence::default()
    };
    let control = |y30: Option<f64>| GrowthEvidence {
        opportunity_id: Some(format!("{template}:target:post:ctx")),
        target_key: Some(target.to_owned()),
        treatment: TreatmentAssignment::Control,
        experiment_uuid: Some(experiment),
        evidence_quality: EvidenceQuality::RandomizedHoldout,
        final_contamination: Some(0.0),
        // Y14 is resolved in both runs; only the Y30 horizon differs.
        observed_incremental_fans: Some(0.0),
        observed_fans: Some(0.0),
        durable_fans_30d: y30,
        action_id: None,
        ..GrowthEvidence::default()
    };

    let y30_effect_after = |control_y30: Option<f64>| {
        let mut model = CausalModel::new();
        apply_evidence_to_model(&mut model, &[control(control_y30), treated.clone()]);
        model
            .predict_stats_with_treatment_for_target(template, Some(target), &ctx)
            .treatment_effect_y30
    };

    let with_contrast = y30_effect_after(Some(0.0));
    let without_contrast = y30_effect_after(None);

    assert!(
        with_contrast > 0.0,
        "the fixture must actually teach the Y30 posterior, got {with_contrast}"
    );
    assert!(
        without_contrast < with_contrast,
        "a Y30 outcome with no Y30 control mean must carry less weight than one \
         with a contrast — same tau, weaker evidence. contrast {with_contrast} \
         vs no contrast {without_contrast}"
    );
}

/// The Y14-to-Y30 bridge is fitted on pairs that carry the same contrast.
///
/// The bridge's slope is what converts a Y14 effect into a Y30 one. Fitting it
/// on a control-adjusted Y30 against a raw Y14 fits the slope between two
/// differently-defined quantities, and the bridged regime then applies that
/// slope to real predictions. A pair that cannot be defined consistently is not
/// weak evidence for the slope; it is evidence for a different slope, so the
/// learner skips it rather than downweighting it.
#[test]
fn the_bridge_skips_a_pair_whose_horizons_disagree_about_the_contrast() {
    use crowdrelay_brain::EvidenceQuality;

    let template = "community-engager";
    let target = "community:aaaaaaaa-0000-0000-0000-000000000003";
    let experiment = uuid::Uuid::from_u128(0x5eed_0002);
    let ctx = DispatchContext::default();

    let treated = GrowthEvidence {
        opportunity_id: Some(format!("{template}:target:post:ctx")),
        target_key: Some(target.to_owned()),
        treatment: TreatmentAssignment::Treatment,
        experiment_uuid: Some(experiment),
        evidence_quality: EvidenceQuality::RandomizedHoldout,
        final_contamination: Some(0.0),
        observed_incremental_fans: Some(4.0),
        observed_fans: Some(4.0),
        durable_fans_30d: Some(10.0),
        predicted_fans: 2.0,
        ..GrowthEvidence::default()
    };
    // Y14 resolved, Y30 pending: the mid-window state every experiment passes
    // through, and the one that produced a mismatched pair.
    let half_resolved_control = GrowthEvidence {
        opportunity_id: Some(format!("{template}:target:post:ctx")),
        target_key: Some(target.to_owned()),
        treatment: TreatmentAssignment::Control,
        experiment_uuid: Some(experiment),
        evidence_quality: EvidenceQuality::RandomizedHoldout,
        final_contamination: Some(0.0),
        observed_incremental_fans: Some(1.0),
        observed_fans: Some(1.0),
        durable_fans_30d: None,
        action_id: None,
        ..GrowthEvidence::default()
    };

    let mut model = CausalModel::new();
    let before = model
        .predict_stats_with_treatment_for_target(template, Some(target), &ctx)
        .bridge_confidence;
    apply_evidence_to_model(&mut model, &[half_resolved_control, treated]);
    let after = model
        .predict_stats_with_treatment_for_target(template, Some(target), &ctx)
        .bridge_confidence;

    assert_eq!(
        after, before,
        "the bridge must not be fitted on a control-adjusted Y30 paired with a \
         raw Y14 — that slope does not describe either quantity"
    );
}

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
