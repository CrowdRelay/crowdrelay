//! The per-video scorecard's pure half.
//!
//! The plan is "every new video +1000 CrowdRelay-driven views in 14 days". This
//! module owns the numbers a card derives from raw rows: which external-traffic
//! domain counts as CrowdRelay-driven, the pace we expect by a given age, the
//! coarse pace label, and the ordered list of what is missing when the video is
//! not getting there. The SQL that gathers those rows lives in
//! `crowdrelay_infra::content_scorecard`.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// Attributed views a video should collect in its first window.
pub const VIDEO_VIEW_TARGET: u64 = 1000;
/// Days a new video gets to reach the target.
pub const VIDEO_WINDOW_DAYS: i64 = 14;
/// Press pitches the outreach lane sends per day, so a seeded queue drains at
/// this rate.
pub const PRESS_PITCHES_PER_DAY: u32 = 40;
/// Age at which "less than half of expected" starts meaning Behind — before
/// two days the expected line is too small to judge.
const PACE_JUDGEMENT_MIN_AGE_DAYS: i64 = 2;

/// Does `domain` count as traffic we drove? A source counts its own domain and
/// any subdomain of it: we touched `youtube.com`, and YouTube Analytics reports
/// `m.youtube.com`.
pub fn attributable_domain(domain: &str, touched: &[String]) -> bool {
    let domain = domain.trim().to_lowercase();
    if domain.is_empty() {
        return false;
    }
    touched.iter().any(|base| {
        let base = base.trim().to_lowercase();
        !base.is_empty() && (domain == base || domain.ends_with(&format!(".{base}")))
    })
}

/// The attributable-view count we expect by `age_days`: the target spread evenly
/// over the window, clamped to it.
pub fn expected_by(age_days: i64) -> u64 {
    VIDEO_VIEW_TARGET.saturating_mul(age_days.clamp(0, VIDEO_WINDOW_DAYS) as u64)
        / VIDEO_WINDOW_DAYS as u64
}

/// The coarse pace label a card shows.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pace {
    /// Attributed views are at least half of what the age expects, or the video
    /// is too young to judge.
    OnTrack,
    /// Attributed views are less than half of expected once the video is at
    /// least two days old.
    Behind,
    /// No `traffic:*` series exists for the source, so attribution cannot be
    /// told from zero.
    Unmeasured,
    /// The video is past its window; the goal is decided.
    Closed,
}

/// Pace from the attributed count, the tracked-click floor, and age. `clicks`
/// is carried so the caller's card can show the floor alongside the pace; it
/// does not change the label — clicks measure our reach, not what viewers did
/// next.
pub fn pace(attributed: Option<u64>, _clicks: u64, age: Duration) -> Pace {
    if age > Duration::days(VIDEO_WINDOW_DAYS) {
        return Pace::Closed;
    }
    let Some(attributed) = attributed else {
        return Pace::Unmeasured;
    };
    if age >= Duration::days(PACE_JUDGEMENT_MIN_AGE_DAYS)
        && attributed.saturating_mul(2) < expected_by(age.whole_days())
    {
        Pace::Behind
    } else {
        Pace::OnTrack
    }
}

/// One thing standing between the video and its goal. The card lists these in
/// enum order, most structural first.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum MissingReason {
    /// Reddit is closed for us — every community play is halted until then.
    RedditHalted {
        #[serde(with = "crate::wire_time")]
        until: OffsetDateTime,
    },
    /// Operator-posted community posts still need their links marked as made.
    ManualPostsWaiting { count: u32 },
    /// No release plan exists for this video at all.
    NoReleasePlan,
    /// A plan exists but its press wave never seeded opportunities.
    PressNotSeeded,
    /// Press opportunities are seeded and waiting on the send lane's cap.
    PressQueued { remaining: u32, per_day_cap: u32 },
    /// Press replies arrived and nobody has answered them.
    PressRepliesUnanswered { count: u32 },
    /// The YouTube connection lacks the Analytics scope, so external traffic
    /// can never be measured.
    NoAnalyticsGrant,
    /// Fan-email deliveries were recorded as undelivered.
    FanEmailUndelivered { count: u32 },
    /// Creator/playlist targets sit in the queue unsent.
    CuratorQueueUnsent { count: u32 },
    /// YouTube reply drafts were approved but never posted.
    ApprovedYoutubeRepliesBlocked { count: u32 },
}

/// Raw counts the scorecard's SQL gathers, which `missing_reasons` turns into
/// the ordered list.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VideoScorecardFacts {
    /// `Some` while Reddit standing keeps every community play halted.
    pub reddit_halted_until: Option<OffsetDateTime>,
    /// `content_source_posts` rows the operator has not marked as posted.
    pub manual_posts_waiting: u32,
    /// An active release plan exists for the video.
    pub has_release_plan: bool,
    /// The plan's press wave seeded at least one opportunity.
    pub press_seeded: bool,
    /// Seeded press opportunities with no outbound interaction yet.
    pub press_queued: u32,
    /// Inbound press replies with no response logged.
    pub press_replies_unanswered: u32,
    /// The YouTube connection grants the Analytics scope.
    pub has_analytics_grant: bool,
    /// Campaign deliveries recorded as undelivered for this release.
    pub fan_email_undelivered: u32,
    /// Creator/playlist/press/radio targets still unsent for the release.
    pub curator_queue_unsent: u32,
    /// YouTube reply drafts approved but not posted.
    pub approved_youtube_replies_blocked: u32,
}

/// The ordered reasons this video is not yet at its goal. Skips anything that
/// is fine; a scorecard with no blockers returns an empty list.
pub fn missing_reasons(facts: &VideoScorecardFacts) -> Vec<MissingReason> {
    let mut reasons = Vec::new();
    if let Some(until) = facts.reddit_halted_until {
        reasons.push(MissingReason::RedditHalted { until });
    }
    if facts.manual_posts_waiting > 0 {
        reasons.push(MissingReason::ManualPostsWaiting {
            count: facts.manual_posts_waiting,
        });
    }
    if !facts.has_release_plan {
        reasons.push(MissingReason::NoReleasePlan);
    } else if !facts.press_seeded {
        reasons.push(MissingReason::PressNotSeeded);
    } else if facts.press_queued > 0 {
        reasons.push(MissingReason::PressQueued {
            remaining: facts.press_queued,
            per_day_cap: PRESS_PITCHES_PER_DAY,
        });
    }
    if facts.press_replies_unanswered > 0 {
        reasons.push(MissingReason::PressRepliesUnanswered {
            count: facts.press_replies_unanswered,
        });
    }
    if !facts.has_analytics_grant {
        reasons.push(MissingReason::NoAnalyticsGrant);
    }
    if facts.fan_email_undelivered > 0 {
        reasons.push(MissingReason::FanEmailUndelivered {
            count: facts.fan_email_undelivered,
        });
    }
    if facts.curator_queue_unsent > 0 {
        reasons.push(MissingReason::CuratorQueueUnsent {
            count: facts.curator_queue_unsent,
        });
    }
    if facts.approved_youtube_replies_blocked > 0 {
        reasons.push(MissingReason::ApprovedYoutubeRepliesBlocked {
            count: facts.approved_youtube_replies_blocked,
        });
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn touched() -> Vec<String> {
        vec!["youtube.com".to_string(), "discord.gg".to_string()]
    }

    #[test]
    fn attribution_counts_exact_domains_and_subdomains() {
        let touched = touched();
        assert!(attributable_domain("youtube.com", &touched));
        assert!(attributable_domain("m.youtube.com", &touched));
        assert!(attributable_domain("YOUTUBE.COM", &touched));
        assert!(attributable_domain("discord.gg", &touched));
        assert!(!attributable_domain("notyoutube.com", &touched));
        assert!(!attributable_domain("youtube.com.evil.example", &touched));
        assert!(!attributable_domain("", &touched));
        assert!(!attributable_domain("youtube.com", &[]));
    }

    #[test]
    fn expected_spreads_the_target_over_the_window() {
        assert_eq!(expected_by(0), 0);
        assert_eq!(expected_by(1), 71);
        assert_eq!(expected_by(7), 500);
        assert_eq!(expected_by(14), 1000);
        assert_eq!(expected_by(30), 1000);
    }

    #[test]
    fn pace_is_unmeasured_without_attribution_while_open() {
        assert_eq!(
            pace(None, 0, Duration::days(5)),
            Pace::Unmeasured,
            "no traffic series can never mean zero"
        );
        assert_eq!(pace(None, 40, Duration::days(30)), Pace::Closed);
    }

    #[test]
    fn pace_is_behind_below_half_of_expected_once_old_enough() {
        // Day 7 expects 500; 249 attributed is behind, 250 is on track.
        assert_eq!(pace(Some(249), 0, Duration::days(7)), Pace::Behind);
        assert_eq!(pace(Some(250), 0, Duration::days(7)), Pace::OnTrack);
    }

    #[test]
    fn pace_gives_videos_two_days_before_judging() {
        assert_eq!(pace(Some(0), 0, Duration::days(1)), Pace::OnTrack);
        assert_eq!(pace(Some(0), 0, Duration::days(3)), Pace::Behind);
    }

    #[test]
    fn pace_closes_after_the_window_regardless_of_count() {
        assert_eq!(
            pace(Some(0), 0, Duration::days(14) + Duration::hours(1)),
            Pace::Closed
        );
        assert_eq!(pace(Some(1500), 0, Duration::days(20)), Pace::Closed);
        assert_eq!(pace(Some(0), 0, Duration::days(14)), Pace::Behind);
    }

    #[test]
    fn missing_reasons_report_only_what_is_missing() {
        let until = datetime!(2026-10-20 00:00 UTC);
        let reasons = missing_reasons(&VideoScorecardFacts {
            reddit_halted_until: Some(until),
            manual_posts_waiting: 9,
            has_release_plan: true,
            press_seeded: true,
            press_queued: 0,
            has_analytics_grant: true,
            ..Default::default()
        });
        assert_eq!(
            reasons,
            vec![
                MissingReason::RedditHalted { until },
                MissingReason::ManualPostsWaiting { count: 9 },
            ]
        );
    }

    #[test]
    fn missing_reasons_walk_the_press_ladder() {
        let facts = VideoScorecardFacts {
            has_release_plan: true,
            has_analytics_grant: true,
            ..Default::default()
        };
        assert_eq!(
            missing_reasons(&facts),
            vec![MissingReason::PressNotSeeded],
            "a plan without opportunities means the press wave never seeded"
        );

        let queued = VideoScorecardFacts {
            press_seeded: true,
            press_queued: 37,
            ..facts.clone()
        };
        assert_eq!(
            missing_reasons(&queued),
            vec![MissingReason::PressQueued {
                remaining: 37,
                per_day_cap: PRESS_PITCHES_PER_DAY
            }]
        );
    }

    #[test]
    fn missing_reasons_cover_the_remaining_supply_gaps() {
        let reasons = missing_reasons(&VideoScorecardFacts {
            has_release_plan: true,
            press_seeded: true,
            press_replies_unanswered: 2,
            has_analytics_grant: false,
            fan_email_undelivered: 7,
            curator_queue_unsent: 31,
            approved_youtube_replies_blocked: 4,
            ..Default::default()
        });
        assert_eq!(
            reasons,
            vec![
                MissingReason::PressRepliesUnanswered { count: 2 },
                MissingReason::NoAnalyticsGrant,
                MissingReason::FanEmailUndelivered { count: 7 },
                MissingReason::CuratorQueueUnsent { count: 31 },
                MissingReason::ApprovedYoutubeRepliesBlocked { count: 4 },
            ]
        );
        assert!(
            missing_reasons(&VideoScorecardFacts {
                has_release_plan: true,
                press_seeded: true,
                has_analytics_grant: true,
                ..Default::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn missing_reason_serializes_with_snake_case_tag() {
        let until = datetime!(2026-10-20 12:30 UTC);
        let json = serde_json::to_value(MissingReason::RedditHalted { until }).unwrap();
        assert_eq!(json["reason"], "reddit_halted");
        assert!(json["until"].as_str().unwrap().starts_with("2026-10-20"));
        let json = serde_json::to_value(MissingReason::PressQueued {
            remaining: 3,
            per_day_cap: 40,
        })
        .unwrap();
        assert_eq!(json["reason"], "press_queued");
        assert_eq!(json["remaining"], 3);
    }
}
