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
    /// The editorial pitch has been put in front of a human once. Parking is
    /// all the agent can do: the Spotify for Artists form has no API, and an
    /// agent that reported it as submitted would be reporting a fiction.
    pub editorial_pitch_parked: bool,
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
    /// Somebody says they submitted the form. The only way this becomes true,
    /// because nothing the agent can read would tell it.
    pub editorial_pitch_done: bool,
    /// When the agent last nudged about it, so a reminder is a reminder rather
    /// than a stream.
    pub editorial_pitch_escalated_at: Option<OffsetDateTime>,
    pub history: ReleaseMilestoneHistory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ReleaseAutopilotPolicy {
    pub calendar_lead_days: u32,
    /// How far before the release the editorial pitch is parked for a human.
    pub editorial_pitch_days_before: u32,
    /// Inside this many days of the release, an unfinished pitch is chased.
    pub editorial_pitch_escalate_within_days: u32,
    /// How long between chases. A reminder every cycle is not a reminder.
    pub editorial_pitch_escalation_cooldown_hours: u32,
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
            // Before the distributor delivers the track, which is what the
            // pitch has to precede. Earlier than the announcement on purpose:
            // the deadline belongs to somebody else's platform and does not
            // move.
            editorial_pitch_days_before: 28,
            editorial_pitch_escalate_within_days: 10,
            editorial_pitch_escalation_cooldown_hours: 48,
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
    /// Assemble the Spotify editorial pitch and park it for a human, with the
    /// deadline attached.
    ///
    /// The form is one per release inside Spotify for Artists and there is no
    /// API for it. Assembling the text and the evidence, working out the
    /// deadline and refusing to let it slip quietly is most of the work and all
    /// of the discipline; pressing submit is not something the agent can do or
    /// should pretend to.
    EditorialPitch,
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
            Self::EditorialPitch => "release.editorial_pitch.v1",
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
    /// Chase an editorial pitch that is still not submitted with the deadline
    /// coming. Separate from `Request` because it repeats: the pitch is parked
    /// once, and then it is somebody's job until they say it is done.
    EscalateEditorialPitch {
        due_at: OffsetDateTime,
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
    // The editorial pitch is handled before the ladder because its deadline is
    // somebody else's and does not move. It falls through when there is nothing
    // to do, so an ordinary release week is unaffected.
    let pitch_due_at =
        snapshot.release_at - Duration::days(i64::from(policy.editorial_pitch_days_before));
    if !snapshot.editorial_pitch_done && until > Duration::ZERO {
        if !snapshot.history.editorial_pitch_parked && now >= pitch_due_at {
            return request(ReleaseMilestone::EditorialPitch, 9_900);
        }
        if snapshot.history.editorial_pitch_parked
            && until <= Duration::days(i64::from(policy.editorial_pitch_escalate_within_days))
        {
            let cooled = snapshot.editorial_pitch_escalated_at.is_none_or(|last| {
                now >= last
                    + Duration::hours(i64::from(policy.editorial_pitch_escalation_cooldown_hours))
            });
            if cooled {
                // Outranks the countdown post it overlaps with. A countdown can
                // go out a day late; this window closes and does not reopen.
                return ReleaseDecision::EscalateEditorialPitch {
                    due_at: pitch_due_at,
                    confidence: Confidence::saturating_from_basis_points(9_800),
                };
            }
        }
    }
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
    policy.editorial_pitch_days_before > policy.editorial_pitch_escalate_within_days
        && policy.editorial_pitch_escalation_cooldown_hours > 0
        && policy.calendar_lead_days >= policy.announcement_days_before
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
            // The existing ladder tests are about the ladder. A pitch already
            // marked done keeps it out of their way, and the tests below cover
            // it on its own.
            editorial_pitch_done: true,
            editorial_pitch_escalated_at: None,
            history: ReleaseMilestoneHistory::default(),
        }
    }

    fn pitch_pending(days: i64) -> ReleasePlanSnapshot {
        // Calendar already seeded: that milestone runs first by design, and
        // these tests are about what happens after it.
        let mut pending = snapshot(days);
        pending.editorial_pitch_done = false;
        pending.history.calendar_seeded = true;
        pending
    }

    #[test]
    fn the_editorial_pitch_is_parked_before_the_distributor_delivers() {
        let policy = ReleaseAutopilotPolicy::default();
        // Too early: nothing to pitch about yet.
        assert!(!matches!(
            evaluate_release(&pitch_pending(40), policy, now()),
            ReleaseDecision::Request {
                milestone: ReleaseMilestone::EditorialPitch,
                ..
            }
        ));
        assert!(matches!(
            evaluate_release(&pitch_pending(20), policy, now()),
            ReleaseDecision::Request {
                milestone: ReleaseMilestone::EditorialPitch,
                ..
            }
        ));
    }

    #[test]
    fn a_parked_pitch_is_chased_as_the_deadline_closes_and_only_on_a_cooldown() {
        let policy = ReleaseAutopilotPolicy::default();
        let mut pending = pitch_pending(5);
        pending.history.editorial_pitch_parked = true;
        let decision = evaluate_release(&pending, policy, now());
        let ReleaseDecision::EscalateEditorialPitch { due_at, .. } = decision else {
            panic!("an unfinished pitch inside the window is chased, got {decision:?}");
        };
        assert_eq!(
            due_at,
            pending.release_at - Duration::days(i64::from(policy.editorial_pitch_days_before))
        );
        // A reminder every cycle is not a reminder.
        pending.editorial_pitch_escalated_at = Some(now() - Duration::hours(1));
        assert!(!matches!(
            evaluate_release(&pending, policy, now()),
            ReleaseDecision::EscalateEditorialPitch { .. }
        ));
        pending.editorial_pitch_escalated_at = Some(
            now()
                - Duration::hours(i64::from(policy.editorial_pitch_escalation_cooldown_hours) + 1),
        );
        assert!(matches!(
            evaluate_release(&pending, policy, now()),
            ReleaseDecision::EscalateEditorialPitch { .. }
        ));
    }

    #[test]
    fn a_submitted_pitch_is_never_chased_and_never_blocks_the_ladder() {
        // Only a human can say it is done, and once they have the release week
        // goes back to being an ordinary release week.
        let policy = ReleaseAutopilotPolicy::default();
        let mut done = snapshot(5);
        done.history.calendar_seeded = true;
        done.history.editorial_pitch_parked = true;
        assert!(!matches!(
            evaluate_release(&done, policy, now()),
            ReleaseDecision::EscalateEditorialPitch { .. }
        ));
        // And after the release the window has closed for good.
        let mut past = pitch_pending(-1);
        past.history.editorial_pitch_parked = true;
        assert!(!matches!(
            evaluate_release(&past, policy, now()),
            ReleaseDecision::EscalateEditorialPitch { .. }
        ));
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
