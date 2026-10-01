use super::SocialContentPerformance;

fn post(reach: u64, acquired: u32, retained: u32) -> SocialContentPerformance {
    SocialContentPerformance {
        platform: "instagram".to_owned(),
        media_type: None,
        opening: None,
        reach: Some(reach),
        fans_acquired: acquired,
        fans_activated_within_30d: acquired,
        fan_conversion_per_1000_reach: None,
        fans_observed_30d: acquired,
        fans_retained_30d: retained,
        retention_window_complete: true,
    }
}

#[test]
fn retained_fans_beat_large_acquisition_without_retention() {
    assert!(post(1_000, 5, 2).evidence_rank() > post(100_000, 100, 0).evidence_rank());
}

#[test]
fn reliable_retained_yield_beats_raw_reach_and_volume() {
    assert!(post(1_000, 5, 4).evidence_rank() > post(10_000, 50, 10).evidence_rank());
}

#[test]
fn incomplete_windows_and_small_samples_have_no_rate() {
    let mut example = post(1_000, 5, 2);
    example.retention_window_complete = false;
    assert_eq!(example.retained_fans_per_1000_reach(), None);
    example.retention_window_complete = true;
    example.fans_observed_30d = 3;
    assert_eq!(example.retained_fans_per_1000_reach(), None);
    example.fans_observed_30d = 5;
    example.reach = Some(99);
    assert_eq!(example.retained_fans_per_1000_reach(), None);
    example.reach = Some(1_000);
    assert_eq!(example.retained_fans_per_1000_reach(), Some(2));
}

#[test]
fn one_real_retained_fan_is_not_erased_by_the_rate_floor() {
    assert!(post(20, 1, 1).evidence_rank() > post(100_000, 50, 0).evidence_rank());
    assert_eq!(post(20, 1, 1).retained_fans_per_1000_reach(), None);
}
