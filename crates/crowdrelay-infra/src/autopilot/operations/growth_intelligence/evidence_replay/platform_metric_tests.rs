use super::*;
use crowdrelay_brain::evidence::EvidenceQuality;
use crowdrelay_brain::{CausalModel, GrowthEvidence, TreatmentAssignment};

#[test]
fn release_series_cannot_inherit_a_fan_experiments_causal_quality() {
    let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(60);
    let row = GrowthEvidence {
        opportunity_id: Some("community-engager:target:post:ctx".to_owned()),
        resolved_at: Some(at),
        evidence_quality: EvidenceQuality::RandomizedHoldout,
        treatment: TreatmentAssignment::Treatment,
        experiment_uuid: Some(uuid::Uuid::now_v7()),
        experiment_assignment_id: Some("assigned".to_owned()),
        observed_metrics: [
            ("release_channel_lift:spotify:followers".to_owned(), -10.0),
            ("release_channel_lift:youtube:views".to_owned(), 10000.0),
            ("release_channel_lift".to_owned(), 9990.0),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let replay = |quality| {
        let mut model = CausalModel::new();
        let mut row = row.clone();
        row.evidence_quality = quality;
        apply_evidence_to_model_with_contrast(&mut model, &[row], &[], None);
        serde_json::to_value(model.metric_posteriors).unwrap()
    };
    let randomized = replay(EvidenceQuality::RandomizedHoldout);
    let observational = replay(EvidenceQuality::Observational);
    assert_eq!(
        randomized, observational,
        "assignment of a fan experiment identifies no release-series counterfactual"
    );
    assert_eq!(randomized.as_object().unwrap().len(), 2);
    assert!(randomized.get("release_channel_lift").is_none());
    let mut model = CausalModel::new();
    apply_evidence_to_model_with_contrast(&mut model, &[row.clone()], &[], None);
    let once = serde_json::to_value(&model.metric_posteriors).unwrap();
    apply_evidence_to_model_with_contrast(&mut model, &[row], &[], Some(at));
    assert_eq!(serde_json::to_value(model.metric_posteriors).unwrap(), once);
}
