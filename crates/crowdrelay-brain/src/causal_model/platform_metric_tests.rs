use super::*;
use crate::evidence::EvidenceQuality;

#[test]
fn platform_units_remain_separate_and_legacy_mixed_forecasts_are_quarantined() {
    let mut model = CausalModel::new();
    for (key, value) in [
        ("release_channel_lift:spotify:followers", -10.0),
        ("release_channel_lift:youtube:views", 10000.0),
    ] {
        model.update_metric(
            key,
            Some("community-engager"),
            None,
            value,
            2.0,
            EvidenceQuality::Observational,
        );
    }
    let follower = model
        .predict_metric_stats(
            "release_channel_lift:spotify:followers",
            "community-engager",
            None,
        )
        .unwrap();
    let views = model
        .predict_metric_stats(
            "release_channel_lift:youtube:views",
            "community-engager",
            None,
        )
        .unwrap();
    assert!(follower.0 < 0.0 && views.0 > 0.0);
    // Simulate a checkpoint from before the unit split. Preserve it for
    // inspection, but never expose or extend its mixed-unit prediction.
    let old = model.metric_posteriors["release_channel_lift:youtube:views"].clone();
    model
        .metric_posteriors
        .insert("release_channel_lift".to_owned(), old);
    let before = serde_json::to_value(&model.metric_posteriors["release_channel_lift"]).unwrap();
    model.update_metric(
        "release_channel_lift",
        Some("community-engager"),
        None,
        1e9,
        1.0,
        EvidenceQuality::RandomizedHoldout,
    );
    assert_eq!(
        before,
        serde_json::to_value(&model.metric_posteriors["release_channel_lift"]).unwrap()
    );
    assert!(
        model
            .predict_metric_stats("release_channel_lift", "community-engager", None)
            .is_none()
    );
    let stats = model.predict_stats_with_treatment_for_target(
        "community-engager",
        None,
        &DispatchContext::default(),
    );
    assert!(!stats.secondary.contains_key("release_channel_lift"));
    assert_eq!(stats.secondary.len(), 2);
}
