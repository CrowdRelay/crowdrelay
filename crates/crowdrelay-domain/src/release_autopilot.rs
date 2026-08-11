//! Release campaign bounded context.
//!
//! A release plan is a trusted operator-owned fact. The domain only decides
//! which deterministic milestone is due next; rendering copy and provider I/O
//! remain outside the domain.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{ReleasePlanId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ReleaseMilestoneHistory {
    pub calendar_seeded: bool,
    pub announcement_sent: bool,
    pub press_started: bool,
    pub fan_warmup_sent: bool,
    pub countdown_sent: bool,
    pub release_day_sent: bool,
    pub sustain_sent: bool,
    pub wrap_sent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleasePlanSnapshot {
    pub release_id: ReleasePlanId,
    pub title: String,
    pub release_at: OffsetDateTime,
    pub active: bool,
    pub assets_ready: bool,
    pub communication_enabled: bool,
    pub press_enabled: bool,
    pub history: ReleaseMilestoneHistory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ReleaseAutopilotPolicy {
    pub calendar_lead_days: u32,
    pub announcement_days_before: u32,
    pub press_days_before: u32,
    pub fan_warmup_days_before: u32,
    pub countdown_days_before: u32,
    pub sustain_days_after: u32,
    pub wrap_days_after: u32,
}

impl Default for ReleaseAutopilotPolicy {
    fn default() -> Self {
        Self {
            calendar_lead_days: 42,
            announcement_days_before: 28,
            press_days_before: 21,
            fan_warmup_days_before: 14,
            countdown_days_before: 7,
            sustain_days_after: 3,
            wrap_days_after: 14,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseMilestone {
    SeedCalendar,
    Announcement,
    StartPress,
    FanWarmup,
    Countdown,
    ReleaseDay,
    Sustain,
    Wrap,
}

impl ReleaseMilestone {
    #[must_use]
    pub const fn template_key(self) -> &'static str {
        match self {
            Self::SeedCalendar => "release.calendar.v1",
            Self::Announcement => "release.announcement.v1",
            Self::StartPress => "release.press.v1",
            Self::FanWarmup => "release.fan_warmup.v1",
            Self::Countdown => "release.countdown.v1",
            Self::ReleaseDay => "release.day.v1",
            Self::Sustain => "release.sustain.v1",
            Self::Wrap => "release.wrap.v1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseDecision {
    Hold(ReleaseHoldReason),
    Request {
        milestone: ReleaseMilestone,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseHoldReason {
    InvalidPolicy,
    Inactive,
    AssetsMissing,
    CommunicationDisabled,
    NotDue,
    AlreadyDone,
}

#[must_use]
pub fn evaluate_release(
    snapshot: &ReleasePlanSnapshot,
    policy: ReleaseAutopilotPolicy,
    now: OffsetDateTime,
) -> ReleaseDecision {
    if !valid_policy(policy) {
        return ReleaseDecision::Hold(ReleaseHoldReason::InvalidPolicy);
    }
    if !snapshot.active {
        return ReleaseDecision::Hold(ReleaseHoldReason::Inactive);
    }

    if !snapshot.history.calendar_seeded
        && now >= snapshot.release_at - Duration::days(i64::from(policy.calendar_lead_days))
    {
        return request(ReleaseMilestone::SeedCalendar, 9_900);
    }
    if !snapshot.assets_ready {
        return ReleaseDecision::Hold(ReleaseHoldReason::AssetsMissing);
    }
    if !snapshot.communication_enabled {
        return ReleaseDecision::Hold(ReleaseHoldReason::CommunicationDisabled);
    }

    let until = snapshot.release_at - now;
    if until <= -Duration::days(i64::from(policy.wrap_days_after)) {
        return if snapshot.history.wrap_sent {
            ReleaseDecision::Hold(ReleaseHoldReason::AlreadyDone)
        } else {
            request(ReleaseMilestone::Wrap, 9_500)
        };
    }
    if until <= -Duration::days(i64::from(policy.sustain_days_after)) {
        return if snapshot.history.sustain_sent {
            ReleaseDecision::Hold(ReleaseHoldReason::AlreadyDone)
        } else {
            request(ReleaseMilestone::Sustain, 9_500)
        };
    }
    if until <= Duration::ZERO {
        return if snapshot.history.release_day_sent {
            ReleaseDecision::Hold(ReleaseHoldReason::AlreadyDone)
        } else {
            request(ReleaseMilestone::ReleaseDay, 9_900)
        };
    }
    if until <= Duration::days(i64::from(policy.countdown_days_before)) {
        return if snapshot.history.countdown_sent {
            ReleaseDecision::Hold(ReleaseHoldReason::AlreadyDone)
        } else {
            request(ReleaseMilestone::Countdown, 9_400)
        };
    }
    if until <= Duration::days(i64::from(policy.fan_warmup_days_before)) {
        return if snapshot.history.fan_warmup_sent {
            ReleaseDecision::Hold(ReleaseHoldReason::AlreadyDone)
        } else {
            request(ReleaseMilestone::FanWarmup, 9_200)
        };
    }
    if snapshot.press_enabled && until <= Duration::days(i64::from(policy.press_days_before)) {
        return if snapshot.history.press_started {
            ReleaseDecision::Hold(ReleaseHoldReason::AlreadyDone)
        } else {
            request(ReleaseMilestone::StartPress, 9_100)
        };
    }
    if until <= Duration::days(i64::from(policy.announcement_days_before)) {
        return if snapshot.history.announcement_sent {
            ReleaseDecision::Hold(ReleaseHoldReason::AlreadyDone)
        } else {
            request(ReleaseMilestone::Announcement, 9_300)
        };
    }
    ReleaseDecision::Hold(ReleaseHoldReason::NotDue)
}

const fn request(milestone: ReleaseMilestone, bp: u16) -> ReleaseDecision {
    ReleaseDecision::Request {
        milestone,
        confidence: Confidence::saturating_from_basis_points(bp),
    }
}

const fn valid_policy(policy: ReleaseAutopilotPolicy) -> bool {
    policy.calendar_lead_days >= policy.announcement_days_before
        && policy.announcement_days_before >= policy.press_days_before
        && policy.press_days_before >= policy.fan_warmup_days_before
        && policy.fan_warmup_days_before >= policy.countdown_days_before
        && policy.countdown_days_before > 0
        && policy.wrap_days_after > policy.sustain_days_after
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }
    fn snapshot(days: i64) -> ReleasePlanSnapshot {
        ReleasePlanSnapshot {
            release_id: ReleasePlanId::new(),
            title: "Signal Lost".into(),
            release_at: now() + Duration::days(days),
            active: true,
            assets_ready: true,
            communication_enabled: true,
            press_enabled: true,
            history: ReleaseMilestoneHistory::default(),
        }
    }

    #[test]
    fn release_seeds_calendar_before_any_campaign_action() {
        assert!(matches!(
            evaluate_release(&snapshot(30), ReleaseAutopilotPolicy::default(), now()),
            ReleaseDecision::Request {
                milestone: ReleaseMilestone::SeedCalendar,
                ..
            }
        ));
    }

    #[test]
    fn press_starts_only_after_calendar_is_seeded() {
        let mut s = snapshot(20);
        s.history.calendar_seeded = true;
        s.history.announcement_sent = true;
        assert!(matches!(
            evaluate_release(&s, ReleaseAutopilotPolicy::default(), now()),
            ReleaseDecision::Request {
                milestone: ReleaseMilestone::StartPress,
                ..
            }
        ));
    }
}
