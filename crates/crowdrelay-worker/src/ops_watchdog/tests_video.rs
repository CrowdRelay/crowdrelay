// The video-scorecard conditions, split out of `tests.rs` for the
// source-size ratchet — same `include!` family as `conditions.rs`. The
// helpers (`healthy`, `publishing`) live in the sibling `tests` module.

#[cfg(test)]
mod video_tests {
    use super::tests::{healthy, publishing};
    use super::{MissingReason, OpsSnapshot, Pace, VideoScorecardView, conditions};
    use sqlx::types::Json;
    use time::OffsetDateTime;

    /// One card for the tests: measured, on pace, nothing missing — each
    /// test bends the one field its condition reads.
    fn video_card() -> VideoScorecardView {
        VideoScorecardView {
            source_id: uuid::Uuid::now_v7(),
            source_key: "youtube:abc123def".to_owned(),
            video_id: "abc123def".to_owned(),
            title: "Technophobia".to_owned(),
            url: None,
            published_at: OffsetDateTime::now_utc() - time::Duration::days(7),
            age_days: 7,
            view_target: 1000,
            window_days: 14,
            expected_by_now: 500,
            attributed_views: Some(400),
            total_views: Some(1200),
            ads_views: Some(50),
            analytics_through: Some(OffsetDateTime::now_utc()),
            pace: Pace::OnTrack,
            tracked_clicks: Default::default(),
            acquired_fans: 0,
            fan_conversion_basis_points: None,
            sends: Default::default(),
            missing: Vec::new(),
            reddit: crowdrelay_application::autopilot::VideoRedditStanding {
                open: true,
                daily_cap: 1,
                hold_reason: None,
                halted_until: None,
            },
        }
    }

    /// `video.behind_pace` fires on the card's own verdict — a video under
    /// half of expected inside its window — and hands the operator the
    /// title, the count, and the top blockers.
    #[test]
    fn a_video_behind_its_pace_raises_attention() {
        let find = |snapshot: &OpsSnapshot| {
            conditions(snapshot, publishing())
                .into_iter()
                .find(|c| c.key == "video.behind_pace")
                .expect("the condition is evaluated")
        };
        assert!(!find(&healthy()).active);

        let mut snapshot = healthy();
        let mut card = video_card();
        card.pace = Pace::Behind;
        card.attributed_views = Some(120);
        card.missing = vec![
            MissingReason::PressNotSeeded,
            MissingReason::FanEmailUndelivered { count: 3 },
        ];
        snapshot.video_cards = Json(vec![card]);
        let raised = find(&snapshot);
        assert!(raised.active);
        assert_eq!(raised.severity, "warning");
        assert_eq!(raised.details["behind"][0]["title"], "Technophobia");
        assert_eq!(raised.details["behind"][0]["attributed_views"], 120);
        assert_eq!(raised.details["behind"][0]["expected_by_now"], 500);
        assert_eq!(
            raised.details["behind"][0]["missing"],
            serde_json::json!(["press_not_seeded", "fan_email_undelivered"])
        );
    }

    /// `video.unmeasured` is a note, not an alarm: the Analytics grant is
    /// missing, so nothing can separate sent views from organic ones.
    #[test]
    fn a_video_without_analytics_reports_unmeasured() {
        let find = |snapshot: &OpsSnapshot| {
            conditions(snapshot, publishing())
                .into_iter()
                .find(|c| c.key == "video.unmeasured")
                .expect("the condition is evaluated")
        };
        assert!(!find(&healthy()).active);

        let mut snapshot = healthy();
        let mut card = video_card();
        card.pace = Pace::Unmeasured;
        card.attributed_views = None;
        card.missing = vec![MissingReason::NoAnalyticsGrant];
        snapshot.video_cards = Json(vec![card]);
        let raised = find(&snapshot);
        assert!(raised.active);
        assert_eq!(raised.severity, "info");
        assert_eq!(raised.details["videos_unmeasured"][0], "Technophobia");
    }
}
