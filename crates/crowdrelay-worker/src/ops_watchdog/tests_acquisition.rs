// The fan-funnel and parked-draft conditions, split out of `tests.rs` for
// the source-size ratchet — same `include!` family as `tests_video.rs`.
// The helpers (`healthy`, `publishing`) live in the sibling `tests` module.

#[cfg(test)]
mod acquisition_tests {
    use super::tests::{healthy, publishing};
    use super::{OpsSnapshot, conditions};
    use serde_json::json;

    /// `fans.acquisition_stalled` fires a day past the seven-day window and
    /// stays silent inside it — the boundary is the last join's age.
    #[test]
    fn a_week_without_a_fan_raises_attention() {
        let find = |snapshot: &OpsSnapshot| {
            conditions(snapshot, publishing())
                .into_iter()
                .find(|c| c.key == "fans.acquisition_stalled")
                .expect("the condition is evaluated")
        };
        // Six days since the last join: still inside the window.
        let mut snapshot = healthy();
        snapshot.last_fan_at =
            Some(time::OffsetDateTime::now_utc() - time::Duration::days(6));
        assert!(!find(&snapshot).active);

        // A day past it, with clicks arriving that converted nobody.
        snapshot.last_fan_at =
            Some(time::OffsetDateTime::now_utc() - time::Duration::days(8));
        snapshot.clicks_7d = 41;
        snapshot.clicks_7d_by_channel = Some(json!({"telegram": 30, "reddit": 5, "youtube": 6}));
        let raised = find(&snapshot);
        assert!(raised.active);
        assert_eq!(raised.severity, "warning");
        assert_eq!(raised.details["clicks_7d"], 41);
        // Only capture channels count — youtube's six are not part of it.
        assert_eq!(raised.details["capture_clicks_7d"], 35);
    }

    /// `fans.capture_disabled` fires only when the page is unconfigured AND
    /// capture-eligible clicks arrived. Non-capture clicks (youtube) alone,
    /// no clicks at all, or a configured page all stay silent.
    #[test]
    fn capture_clicks_with_no_capture_page_raise_a_critical() {
        let find = |snapshot: &OpsSnapshot| {
            conditions(snapshot, publishing())
                .into_iter()
                .find(|c| c.key == "fans.capture_disabled")
                .expect("the condition is evaluated")
        };
        let mut snapshot = healthy();
        snapshot.capture_page_configured = false;

        // Nothing arrived: the switch would change nothing, so no page.
        assert!(!find(&snapshot).active);

        // Only youtube clicks: they are already on YouTube, not capturable.
        snapshot.clicks_7d_by_channel = Some(json!({"youtube": 12}));
        assert!(!find(&snapshot).active);

        // Capture-eligible clicks went past an unconfigured page.
        snapshot.clicks_7d_by_channel = Some(json!({"telegram": 4, "youtube": 12, "reddit": 3}));
        let raised = find(&snapshot);
        assert!(raised.active);
        assert_eq!(raised.severity, "critical");
        assert_eq!(raised.details["capture_clicks_7d"], 7);

        // Configured: the same clicks land on /watch, so it clears.
        snapshot.capture_page_configured = true;
        assert!(!find(&snapshot).active);
    }

    /// Nobody ever joining is the stall at its most extreme.
    #[test]
    fn never_having_acquired_a_fan_is_the_stall() {
        let mut snapshot = healthy();
        snapshot.last_fan_at = None;
        let raised = conditions(&snapshot, publishing())
            .into_iter()
            .find(|c| c.key == "fans.acquisition_stalled")
            .expect("the condition is evaluated");
        assert!(raised.active);
        assert!(raised.details["last_fan_at"].is_null());
    }

    /// `social.manual_posts_stale` fires on a 49-hour parked draft on an
    /// autopost platform, silent at 47 hours, silent on a platform the
    /// setting does not cover, silent once nothing is parked at all.
    #[test]
    fn a_stale_park_on_an_autopost_platform_raises_attention() {
        let find = |snapshot: &OpsSnapshot| {
            conditions(snapshot, publishing())
                .into_iter()
                .find(|c| c.key == "social.manual_posts_stale")
                .expect("the condition is evaluated")
        };

        // 47 hours is inside the grace even on an autopost platform.
        let mut snapshot = healthy();
        snapshot.parked_manual_posts = Some(json!([{"platform": "instagram", "age_hours": 47.0}]));
        assert!(!find(&snapshot).active);

        // Past it: instagram is an autopost platform by default.
        snapshot.parked_manual_posts = Some(json!([
            {"platform": "instagram", "age_hours": 49.0},
            {"platform": "instagram", "age_hours": 52.5},
        ]));
        let raised = find(&snapshot);
        assert!(raised.active);
        assert_eq!(raised.severity, "warning");
        assert_eq!(raised.details["stale_by_platform"]["instagram"]["count"], 2);
        assert_eq!(
            raised.details["stale_by_platform"]["instagram"]["oldest_age_hours"],
            52.5
        );

        // A platform the tenant's setting does not cover is a human lane —
        // parked there is by design and never raises the alarm.
        let mut manual_lane = healthy();
        manual_lane.social_autopost_platforms_setting = Some("telegram".to_owned());
        manual_lane.parked_manual_posts =
            Some(json!([{"platform": "instagram", "age_hours": 72.0}]));
        assert!(!find(&manual_lane).active);

        // Posted — nothing parked — is silent too.
        assert!(!find(&healthy()).active);
    }
}
