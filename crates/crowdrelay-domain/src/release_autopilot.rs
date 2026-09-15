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

/// The band's call about what kind of release this is (§4i-3). Recording it
/// on the plan is what lets timing and outcome be compared across releases
/// from the first one.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseTier {
    /// The full vertical: every milestone, pre-save, push.
    Single,
    /// Announced, catalogued, one wave. The honest default.
    #[default]
    Track,
    /// Posted into a quiet week. No vertical, no spend — the content-supply
    /// evaluator is what posts it.
    Filler,
}

impl ReleaseTier {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Track => "track",
            Self::Filler => "filler",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "single" => Some(Self::Single),
            "track" => Some(Self::Track),
            "filler" => Some(Self::Filler),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleasePlanSnapshot {
    pub release_id: ReleasePlanId,
    pub title: String,
    pub release_at: OffsetDateTime,
    pub active: bool,
    pub tier: ReleaseTier,
    pub assets_ready: bool,
    pub communication_enabled: bool,
    pub press_enabled: bool,
    /// Somebody says they submitted the form, and when. The only way this
    /// becomes set, because nothing the agent can read would tell it.
    pub editorial_pitch_completed_at: Option<OffsetDateTime>,
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
    /// The storage vocabulary, matching the milestone CHECK constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SeedCalendar => "seed_calendar",
            Self::EditorialPitch => "editorial_pitch",
            Self::Announcement => "announcement",
            Self::StartPress => "start_press",
            Self::FanWarmup => "fan_warmup",
            Self::Countdown => "countdown",
            Self::ReleaseDay => "release_day",
            Self::Sustain => "sustain",
            Self::Wrap => "wrap",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "seed_calendar" => Some(Self::SeedCalendar),
            "editorial_pitch" => Some(Self::EditorialPitch),
            "announcement" => Some(Self::Announcement),
            "start_press" => Some(Self::StartPress),
            "fan_warmup" => Some(Self::FanWarmup),
            "countdown" => Some(Self::Countdown),
            "release_day" => Some(Self::ReleaseDay),
            "sustain" => Some(Self::Sustain),
            "wrap" => Some(Self::Wrap),
            _ => None,
        }
    }

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
    /// A filler release is posted into a quiet week and owes no vertical —
    /// that is what the tier is for, not a failure to schedule.
    TierFiller,
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
    if snapshot.tier == ReleaseTier::Filler {
        return ReleaseDecision::Hold(ReleaseHoldReason::TierFiller);
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
    if snapshot.editorial_pitch_completed_at.is_none() && until > Duration::ZERO {
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

/// How far the release itself has come (§4i-1): the read-model phase an
/// openable timeline header renders.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleasePhase {
    /// The plan exists but is switched off.
    Inactive,
    /// R-42 and counting: the ladder is running or has not started.
    Preparing,
    /// Inside the countdown window — the release week itself.
    ReleaseWeek,
    /// Out the other side, into the sustain and wrap stretch.
    Sustaining,
    /// The wrap is recorded or its window has fully passed.
    Complete,
}

/// Where one rung of the ladder actually got to.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStepState {
    Done,
    /// The editorial pitch is parked in front of a human: parked, not
    /// submitted — the deadline is theirs and the form has no API.
    Parked,
    /// Its window is open and nothing recorded it.
    Due,
    /// Still ahead of its window.
    Upcoming,
    /// The step's own switch is off — press on a no-press release.
    Disabled,
    /// A plan-level gate holds it: missing assets or communication switched
    /// off. Same holds the evaluator answers with.
    Blocked,
}

/// One rung of the release ladder as a page renders it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ReleaseTimelineStep {
    pub milestone: ReleaseMilestone,
    /// Days relative to the release: -28 is R-28, 3 is R+3.
    pub offset_days: i32,
    pub due_at: OffsetDateTime,
    /// When the step's work actually finished. For the editorial pitch that
    /// is the human's say-so, not the parking — parked is a state, not a
    /// completion.
    pub completed_at: Option<OffsetDateTime>,
    pub state: ReleaseStepState,
}

/// The ladder as a page can open it: every milestone, its R-offset, its due
/// date and where it actually got to — computed from the same policy and the
/// same facts the evaluator decides from, so the view cannot drift from the
/// decisions.
#[must_use]
pub fn release_timeline(
    snapshot: &ReleasePlanSnapshot,
    completed: &[(ReleaseMilestone, OffsetDateTime)],
    policy: ReleaseAutopilotPolicy,
    now: OffsetDateTime,
) -> Vec<ReleaseTimelineStep> {
    let at = |days: i64| snapshot.release_at + Duration::days(days);
    let rungs: [(ReleaseMilestone, i64); 9] = [
        (
            ReleaseMilestone::SeedCalendar,
            -i64::from(policy.calendar_lead_days),
        ),
        (
            ReleaseMilestone::EditorialPitch,
            -i64::from(policy.editorial_pitch_days_before),
        ),
        (
            ReleaseMilestone::Announcement,
            -i64::from(policy.announcement_days_before),
        ),
        (
            ReleaseMilestone::StartPress,
            -i64::from(policy.press_days_before),
        ),
        (
            ReleaseMilestone::FanWarmup,
            -i64::from(policy.fan_warmup_days_before),
        ),
        (
            ReleaseMilestone::Countdown,
            -i64::from(policy.countdown_days_before),
        ),
        (ReleaseMilestone::ReleaseDay, 0),
        (
            ReleaseMilestone::Sustain,
            i64::from(policy.sustain_days_after),
        ),
        (ReleaseMilestone::Wrap, i64::from(policy.wrap_days_after)),
    ];
    rungs
        .iter()
        .map(|&(milestone, offset)| {
            let due_at = at(offset);
            let completed_at = if milestone == ReleaseMilestone::EditorialPitch {
                // Parked is the milestone row; submitted is the completion —
                // only the human's timestamp finishes this step.
                snapshot.editorial_pitch_completed_at
            } else {
                completed
                    .iter()
                    .find(|(m, _)| *m == milestone)
                    .map(|(_, at)| *at)
            };
            let state = step_state(snapshot, milestone, completed_at, due_at, now);
            ReleaseTimelineStep {
                milestone,
                offset_days: i32::try_from(offset).unwrap_or(i32::MAX),
                due_at,
                completed_at,
                state,
            }
        })
        .collect()
}

fn step_state(
    snapshot: &ReleasePlanSnapshot,
    milestone: ReleaseMilestone,
    completed_at: Option<OffsetDateTime>,
    due_at: OffsetDateTime,
    now: OffsetDateTime,
) -> ReleaseStepState {
    if completed_at.is_some() {
        return ReleaseStepState::Done;
    }
    if milestone == ReleaseMilestone::EditorialPitch && snapshot.history.editorial_pitch_parked {
        return ReleaseStepState::Parked;
    }
    // A filler plan owes no vertical: every unrecorded rung is off by the
    // band's own call, which is what the tier is for — the view must not
    // report a ladder the evaluator would never run.
    if snapshot.tier == ReleaseTier::Filler {
        return ReleaseStepState::Disabled;
    }
    if milestone == ReleaseMilestone::StartPress && !snapshot.press_enabled {
        return ReleaseStepState::Disabled;
    }
    // The evaluator holds everything below the calendar when assets are
    // missing or communication is off — the view says so rather than
    // pretending those steps are merely unscheduled.
    if milestone != ReleaseMilestone::SeedCalendar
        && (!snapshot.assets_ready || !snapshot.communication_enabled)
    {
        return ReleaseStepState::Blocked;
    }
    if now >= due_at {
        return ReleaseStepState::Due;
    }
    ReleaseStepState::Upcoming
}

/// The phase the timeline header renders.
#[must_use]
pub fn release_phase(
    snapshot: &ReleasePlanSnapshot,
    policy: ReleaseAutopilotPolicy,
    now: OffsetDateTime,
) -> ReleasePhase {
    if !snapshot.active {
        return ReleasePhase::Inactive;
    }
    let until = snapshot.release_at - now;
    if until <= -Duration::days(i64::from(policy.wrap_days_after)) || snapshot.history.wrap_sent {
        return ReleasePhase::Complete;
    }
    if until <= -Duration::days(i64::from(policy.sustain_days_after)) {
        return ReleasePhase::Sustaining;
    }
    if until <= Duration::days(i64::from(policy.countdown_days_before)) {
        return ReleasePhase::ReleaseWeek;
    }
    ReleasePhase::Preparing
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
            tier: ReleaseTier::Track,
            assets_ready: true,
            communication_enabled: true,
            press_enabled: true,
            // The existing ladder tests are about the ladder. A pitch already
            // marked done keeps it out of their way, and the tests below cover
            // it on its own.
            editorial_pitch_completed_at: Some(now()),
            editorial_pitch_escalated_at: None,
            history: ReleaseMilestoneHistory::default(),
        }
    }

    fn pitch_pending(days: i64) -> ReleasePlanSnapshot {
        // Calendar already seeded: that milestone runs first by design, and
        // these tests are about what happens after it.
        let mut pending = snapshot(days);
        pending.editorial_pitch_completed_at = None;
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

    #[test]
    fn a_filler_release_owes_no_vertical() {
        let mut filler = snapshot(10);
        filler.tier = ReleaseTier::Filler;
        assert!(matches!(
            evaluate_release(&filler, ReleaseAutopilotPolicy::default(), now()),
            ReleaseDecision::Hold(ReleaseHoldReason::TierFiller)
        ));
        // Singles and tracks still run the ladder.
        assert!(matches!(
            evaluate_release(&snapshot(10), ReleaseAutopilotPolicy::default(), now()),
            ReleaseDecision::Request { .. }
        ));
    }

    #[test]
    fn a_filler_timeline_shows_the_ladder_is_off_not_due() {
        let policy = ReleaseAutopilotPolicy::default();
        let mut filler = snapshot(20);
        filler.tier = ReleaseTier::Filler;
        filler.editorial_pitch_completed_at = None;
        let timeline = release_timeline(&filler, &[], policy, now());
        assert!(
            timeline
                .iter()
                .all(|step| step.state == ReleaseStepState::Disabled)
        );
        // A completion already recorded still reads done — the facts win.
        let completed = [(ReleaseMilestone::SeedCalendar, now())];
        let timeline = release_timeline(&filler, &completed, policy, now());
        let calendar = timeline
            .iter()
            .find(|step| step.milestone == ReleaseMilestone::SeedCalendar)
            .expect("every rung is present");
        assert_eq!(calendar.state, ReleaseStepState::Done);
    }

    #[test]
    fn release_tier_round_trips() {
        for tier in [ReleaseTier::Single, ReleaseTier::Track, ReleaseTier::Filler] {
            assert_eq!(ReleaseTier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(ReleaseTier::parse("album"), None);
    }

    #[test]
    fn every_milestone_round_trips_through_storage_vocabulary() {
        let all = [
            ReleaseMilestone::SeedCalendar,
            ReleaseMilestone::EditorialPitch,
            ReleaseMilestone::Announcement,
            ReleaseMilestone::StartPress,
            ReleaseMilestone::FanWarmup,
            ReleaseMilestone::Countdown,
            ReleaseMilestone::ReleaseDay,
            ReleaseMilestone::Sustain,
            ReleaseMilestone::Wrap,
        ];
        assert_eq!(all.len(), 9);
        for milestone in all {
            assert_eq!(ReleaseMilestone::parse(milestone.as_str()), Some(milestone));
        }
        assert_eq!(ReleaseMilestone::parse("press"), None);
    }

    #[test]
    fn the_timeline_opens_every_rung_with_the_policy_offsets() {
        let policy = ReleaseAutopilotPolicy::default();
        let mut s = snapshot(50);
        s.editorial_pitch_completed_at = None;
        let timeline = release_timeline(&s, &[], policy, now());
        assert_eq!(timeline.len(), 9);
        let offsets: Vec<i32> = timeline.iter().map(|step| step.offset_days).collect();
        assert_eq!(offsets, [-42, -28, -28, -21, -14, -7, 0, 3, 14]);
        // Nothing recorded yet: everything ahead is upcoming, nothing claims
        // a completion that never happened.
        assert!(
            timeline
                .iter()
                .all(|step| step.state == ReleaseStepState::Upcoming && step.completed_at.is_none())
        );
    }

    #[test]
    fn the_timeline_marks_due_done_and_parked_honestly() {
        let policy = ReleaseAutopilotPolicy::default();
        // R-20: calendar, pitch and announcement windows are all open.
        let mut s = snapshot(20);
        s.history.calendar_seeded = true;
        s.history.editorial_pitch_parked = true;
        s.editorial_pitch_completed_at = None;
        let calendar_done = now() - Duration::days(2);
        let completed = [(ReleaseMilestone::SeedCalendar, calendar_done)];
        let timeline = release_timeline(&s, &completed, policy, now());
        let at = |m: ReleaseMilestone| {
            timeline
                .iter()
                .find(|step| step.milestone == m)
                .expect("every rung is present")
        };
        assert_eq!(
            at(ReleaseMilestone::SeedCalendar).completed_at,
            Some(calendar_done)
        );
        assert_eq!(
            at(ReleaseMilestone::SeedCalendar).state,
            ReleaseStepState::Done
        );
        // Parked, not done: the pitch is somebody's job until they say so.
        assert_eq!(
            at(ReleaseMilestone::EditorialPitch).state,
            ReleaseStepState::Parked
        );
        assert_eq!(at(ReleaseMilestone::EditorialPitch).completed_at, None);
        assert_eq!(
            at(ReleaseMilestone::Announcement).state,
            ReleaseStepState::Due
        );
        assert_eq!(at(ReleaseMilestone::Wrap).state, ReleaseStepState::Upcoming);
    }

    #[test]
    fn the_timeline_says_disabled_and_blocked_instead_of_silent() {
        let policy = ReleaseAutopilotPolicy::default();
        let mut s = snapshot(20);
        s.press_enabled = false;
        s.assets_ready = false;
        let timeline = release_timeline(&s, &[], policy, now());
        let at = |m: ReleaseMilestone| {
            timeline
                .iter()
                .find(|step| step.milestone == m)
                .expect("every rung is present")
        };
        assert_eq!(
            at(ReleaseMilestone::StartPress).state,
            ReleaseStepState::Disabled
        );
        // The calendar still fires without assets — everything below it is
        // held by the gate, and the view names the hold rather than hiding it.
        assert_eq!(
            at(ReleaseMilestone::Announcement).state,
            ReleaseStepState::Blocked
        );
        assert_eq!(
            at(ReleaseMilestone::SeedCalendar).state,
            ReleaseStepState::Due
        );
    }

    #[test]
    fn the_phase_walks_the_release_from_preparing_to_complete() {
        let policy = ReleaseAutopilotPolicy::default();
        assert_eq!(
            release_phase(&snapshot(30), policy, now()),
            ReleasePhase::Preparing
        );
        assert_eq!(
            release_phase(&snapshot(3), policy, now()),
            ReleasePhase::ReleaseWeek
        );
        assert_eq!(
            release_phase(&snapshot(-5), policy, now()),
            ReleasePhase::Sustaining
        );
        assert_eq!(
            release_phase(&snapshot(-20), policy, now()),
            ReleasePhase::Complete
        );
        let mut wrapped = snapshot(-5);
        wrapped.history.wrap_sent = true;
        assert_eq!(
            release_phase(&wrapped, policy, now()),
            ReleasePhase::Complete
        );
        let mut off = snapshot(30);
        off.active = false;
        assert_eq!(release_phase(&off, policy, now()), ReleasePhase::Inactive);
    }
}
