//! Arrival order must not prevent durability learning or manufacture samples.

use super::evidence_replay::apply_evidence_to_model_with_contrast;
use crowdrelay_brain::{CausalModel, DispatchContext, GrowthEvidence};
use time::{Duration, OffsetDateTime};

fn replay(model: &mut CausalModel, evidence: &GrowthEvidence, cursor: Option<OffsetDateTime>) {
    apply_evidence_to_model_with_contrast(model, std::slice::from_ref(evidence), &[], cursor);
}

fn pair() -> GrowthEvidence {
    GrowthEvidence {
        opportunity_id: Some("community-engager:target:post:ctx".into()),
        observed_incremental_fans: Some(8.0),
        durable_fans_30d: Some(2.0),
        ..GrowthEvidence::default()
    }
}

#[test]
fn a_bridge_pair_learns_when_the_last_horizon_arrives_in_either_order() {
    let first = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
    let last = first + Duration::days(30);
    for y30_first in [false, true] {
        let mut model = CausalModel::default();
        let mut evidence = pair();
        evidence.replayed_14d_at = Some(if y30_first { last } else { first });
        evidence.replayed_30d_at = Some(if y30_first { first } else { last });
        evidence.resolved_at = Some(last);
        let mut early = evidence.clone();
        early.resolved_at = None;
        if y30_first {
            early.observed_incremental_fans = None;
            early.replayed_14d_at = None;
        } else {
            early.durable_fans_30d = None;
            early.replayed_30d_at = None;
        }
        replay(&mut model, &early, Some(first - Duration::hours(1)));
        assert_eq!(model.bridge.confidence(), 0);
        replay(&mut model, &evidence, Some(first));
        assert_eq!(model.bridge.confidence(), 1);

        let once = serde_json::to_value(&model).unwrap();
        replay(&mut model, &evidence, Some(last));
        assert_eq!(serde_json::to_value(&model).unwrap(), once);

        // A later secondary-metric closure must not count the pair again.
        evidence.resolved_at = Some(last + Duration::hours(1));
        replay(&mut model, &evidence, Some(last));
        assert_eq!(model.bridge.confidence(), 1);

        let mut full = CausalModel::default();
        replay(&mut full, &evidence, None);
        assert_eq!(
            serde_json::to_value(&model.bridge).unwrap(),
            serde_json::to_value(&full.bridge).unwrap(),
            "delta and full replay must fit the same single pair"
        );
    }
}

#[test]
fn legacy_pairs_use_the_resolution_watermark_once() {
    let at = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
    let evidence = GrowthEvidence {
        resolved_at: Some(at),
        ..pair()
    };
    let mut model = CausalModel::default();
    replay(&mut model, &evidence, Some(at - Duration::hours(1)));
    assert_eq!(model.bridge.confidence(), 1);
    replay(&mut model, &evidence, Some(at));
    assert_eq!(model.bridge.confidence(), 1);
}

#[test]
fn durability_learned_across_batches_changes_the_next_decision_value() {
    use crowdrelay_brain::{DecisionMode, DecisionValue, EvidenceQuality, ResourceCost};

    let at = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
    let mut model = CausalModel::default();
    let initial_bridge = serde_json::to_value(&model.bridge).unwrap();
    // Honest, low-quality historical pairs teach durability without buying a
    // direct Y30 regime. A newer template has identified Y14, but not Y30 yet.
    for i in 1..=10 {
        let mut evidence = GrowthEvidence {
            observed_incremental_fans: Some(f64::from(i)),
            replayed_14d_at: Some(at),
            durable_fans_30d: None,
            ..pair()
        };
        replay(&mut model, &evidence, Some(at - Duration::hours(1)));
        evidence.durable_fans_30d = Some(f64::from(i) * 0.2);
        evidence.replayed_30d_at = Some(at + Duration::days(30));
        replay(&mut model, &evidence, Some(at));
    }
    for _ in 0..12 {
        replay(
            &mut model,
            &GrowthEvidence {
                opportunity_id: Some("new-template:target:post:ctx".into()),
                observed_incremental_fans: Some(8.0),
                evidence_quality: EvidenceQuality::MatchedQuasiExperiment,
                ..GrowthEvidence::default()
            },
            None,
        );
    }
    // Hold every other learned quantity fixed to test the bridge's last hop.
    let mut without_durability = serde_json::to_value(&model).unwrap();
    without_durability["bridge"] = initial_bridge;
    let without_durability: CausalModel = serde_json::from_value(without_durability).unwrap();
    let stats = model.predict_stats_with_treatment("new-template", &DispatchContext::default());
    let prior = without_durability
        .predict_stats_with_treatment("new-template", &DispatchContext::default());
    assert!(stats.bridge_is_reliable);
    assert!(stats.use_treatment_effect && !stats.uses_y30);
    assert_ne!(stats.treatment_effect, prior.treatment_effect);
    let value = |stats| {
        DecisionValue::from_stats(stats, ResourceCost::default(), DecisionMode::Exploit).total()
    };
    assert_ne!(value(&stats), value(&prior));
}
