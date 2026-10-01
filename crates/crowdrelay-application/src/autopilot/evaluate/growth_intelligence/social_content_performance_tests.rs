use super::*;

#[test]
fn fan_creating_posts_become_fresh_copy_evidence_not_copy_instructions() {
    let block = social_content_performance_block(&[SocialContentPerformance {
        platform: "instagram".to_owned(),
        media_type: Some("REEL".to_owned()),
        opening: Some("riff first, talk later".to_owned()),
        reach: Some(2_000),
        fans_acquired: 6,
        fans_activated_within_30d: 4,
        fan_conversion_per_1000_reach: Some(3),
        fans_observed_30d: 6,
        fans_retained_30d: 4,
        retention_window_complete: true,
    }]);
    assert!(block.contains("6 acquired fan(s)"));
    assert!(block.contains("4 activated within 30d"));
    assert!(block.contains("D30: 4 retained fan(s)"));
    assert!(block.contains("2 retained fans / 1k reach"));
    assert!(block.contains("3 fans / 1k reach"));
    assert!(block.contains("riff first, talk later"));
    assert!(block.contains("Prioritize patterns that created fans over vanity engagement"));
    assert!(block.contains("do NOT copy or closely paraphrase"));
}

#[test]
fn no_fan_yield_history_adds_no_fake_guidance() {
    assert!(social_content_performance_block(&[]).is_empty());
}

#[test]
fn immature_evidence_never_claims_a_measured_retention_rate() {
    let block = social_content_performance_block(&[SocialContentPerformance {
        platform: "facebook".to_owned(),
        media_type: None,
        opening: None,
        reach: Some(1_000),
        fans_acquired: 5,
        fans_activated_within_30d: 2,
        fan_conversion_per_1000_reach: Some(5),
        fans_observed_30d: 0,
        fans_retained_30d: 0,
        retention_window_complete: false,
    }]);
    assert!(block.contains("retention rate unmeasured"));
    assert!(block.contains("Window incomplete"));
    assert!(!block.contains("0 retained fans / 1k reach"));
}
