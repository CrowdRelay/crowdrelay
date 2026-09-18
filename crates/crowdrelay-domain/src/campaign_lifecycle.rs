//! Event campaign lifecycle bounded context.
//!
//! This module decides *when* a first-party audience campaign is due. Audience
//! selection and delivery stay outside the domain; the result is only a typed
//! lifecycle intent that the application layer may persist for approval/execution.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{EventId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct EventCampaignHistory {
    pub announcement_sent: bool,
    pub interest_reminder_sent: bool,
    pub last_call_sent: bool,
    pub day_of_sent: bool,
    pub thank_you_sent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct EventCampaignSnapshot {
    pub event_id: EventId,
    pub published: bool,
    pub communication_enabled: bool,
    pub starts_at: OffsetDateTime,
    pub interested_fans: u32,
    pub paid_buyers: u32,
    pub attendees: u32,
    pub history: EventCampaignHistory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventCampaignPolicy {
    pub minimum_audience: u32,
    pub announcement_max_days_before: u32,
    pub reminder_days_before: u32,
    pub last_call_hours_before: u32,
    pub day_of_hours_before: u32,
    pub thank_you_hours_after: u32,
}

impl Default for EventCampaignPolicy {
    fn default() -> Self {
        Self {
            minimum_audience: 3,
            announcement_max_days_before: 120,
            reminder_days_before: 21,
            last_call_hours_before: 72,
            day_of_hours_before: 12,
            thank_you_hours_after: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventCampaignPhase {
    Announcement,
    InterestReminder,
    LastCall,
    DayOf,
    ThankYou,
}

impl EventCampaignPhase {
    #[must_use]
    pub const fn template_key(self) -> &'static str {
        match self {
            Self::Announcement => "event.announcement.v1",
            Self::InterestReminder => "event.interest_reminder.v1",
            Self::LastCall => "event.last_call.v1",
            Self::DayOf => "event.day_of.v1",
            Self::ThankYou => "event.thank_you.v1",
        }
    }

    /// Who this phase writes to, in the operator's words (O.5).
    ///
    /// The approval used to name a phase and a template key and nothing else,
    /// so an operator could not tell whether they were about to write to eight
    /// people or eight hundred, nor on what basis those people were reachable.
    /// Every phase has an answer and every answer is a sentence rather than a
    /// segment slug.
    #[must_use]
    pub const fn audience_basis(self) -> &'static str {
        match self {
            Self::Announcement => {
                "every consented fan in the event's city — counted when the campaign is built,                  because the city's audience moves between the approval and the send"
            }
            Self::InterestReminder | Self::LastCall => {
                "fans who said they are coming and have not bought a ticket"
            }
            Self::DayOf => "fans who bought a ticket",
            Self::ThankYou => {
                "fans who were in the room, minus anyone whose scan already got the welcome"
            }
        }
    }

    /// How many people that is, when the decision snapshot already knows.
    ///
    /// `None` for the announcement and only for the announcement: its audience
    /// is every consented fan in the city, which this snapshot does not carry.
    /// Absent is reported as absent — a zero here would read as "nobody" and
    /// stop an announcement that has an audience.
    #[must_use]
    pub fn audience_size(self, snapshot: &EventCampaignSnapshot) -> Option<u32> {
        match self {
            Self::Announcement => None,
            Self::InterestReminder | Self::LastCall => Some(
                snapshot
                    .interested_fans
                    .saturating_sub(snapshot.paid_buyers),
            ),
            Self::DayOf => Some(snapshot.paid_buyers),
            Self::ThankYou => Some(snapshot.attendees),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCampaignDecision {
    Hold(EventCampaignHoldReason),
    Request {
        phase: EventCampaignPhase,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCampaignHoldReason {
    InvalidPolicy,
    Unpublished,
    CommunicationDisabled,
    InsufficientAudience,
    NotDue,
    AlreadySent,
}

#[must_use]
pub fn evaluate_event_campaign(
    snapshot: EventCampaignSnapshot,
    policy: EventCampaignPolicy,
    now: OffsetDateTime,
) -> EventCampaignDecision {
    if !policy_is_valid(policy) {
        return EventCampaignDecision::Hold(EventCampaignHoldReason::InvalidPolicy);
    }
    if !snapshot.published {
        return EventCampaignDecision::Hold(EventCampaignHoldReason::Unpublished);
    }
    if !snapshot.communication_enabled {
        return EventCampaignDecision::Hold(EventCampaignHoldReason::CommunicationDisabled);
    }

    let until = snapshot.starts_at - now;
    if until <= -Duration::hours(i64::from(policy.thank_you_hours_after)) {
        if snapshot.history.thank_you_sent {
            return EventCampaignDecision::Hold(EventCampaignHoldReason::AlreadySent);
        }
        if snapshot.attendees < policy.minimum_audience {
            return EventCampaignDecision::Hold(EventCampaignHoldReason::InsufficientAudience);
        }
        return request(EventCampaignPhase::ThankYou, 9_300);
    }
    // Announcement comes before the narrower pre-event windows because it is
    // the widest gate (up to 120 days) and the later phases only make sense
    // after an announcement has gone out. Without this ordering, an event
    // within `reminder_days_before` but with fewer interested fans than the
    // minimum would hit the InterestReminder guard first and return
    // InsufficientAudience, preventing the announcement from ever firing.
    if until <= Duration::days(i64::from(policy.announcement_max_days_before))
        && until.is_positive()
    {
        if snapshot.history.announcement_sent {
            // Fall through to the narrower pre-event windows below.
        } else {
            return request(EventCampaignPhase::Announcement, 8_800);
        }
    }
    if until <= Duration::hours(i64::from(policy.day_of_hours_before)) && until.is_positive() {
        if snapshot.history.day_of_sent {
            return EventCampaignDecision::Hold(EventCampaignHoldReason::AlreadySent);
        }
        if snapshot.paid_buyers < policy.minimum_audience {
            return EventCampaignDecision::Hold(EventCampaignHoldReason::InsufficientAudience);
        }
        return request(EventCampaignPhase::DayOf, 9_700);
    }
    if until <= Duration::hours(i64::from(policy.last_call_hours_before)) && until.is_positive() {
        if snapshot.history.last_call_sent {
            return EventCampaignDecision::Hold(EventCampaignHoldReason::AlreadySent);
        }
        let unconverted = snapshot
            .interested_fans
            .saturating_sub(snapshot.paid_buyers);
        if unconverted < policy.minimum_audience {
            return EventCampaignDecision::Hold(EventCampaignHoldReason::InsufficientAudience);
        }
        return request(EventCampaignPhase::LastCall, 9_200);
    }
    if until <= Duration::days(i64::from(policy.reminder_days_before)) && until.is_positive() {
        if snapshot.history.interest_reminder_sent {
            return EventCampaignDecision::Hold(EventCampaignHoldReason::AlreadySent);
        }
        let unconverted = snapshot
            .interested_fans
            .saturating_sub(snapshot.paid_buyers);
        if unconverted < policy.minimum_audience {
            return EventCampaignDecision::Hold(EventCampaignHoldReason::InsufficientAudience);
        }
        return request(EventCampaignPhase::InterestReminder, 9_000);
    }
    EventCampaignDecision::Hold(EventCampaignHoldReason::NotDue)
}

fn request(phase: EventCampaignPhase, confidence: u16) -> EventCampaignDecision {
    EventCampaignDecision::Request {
        phase,
        confidence: Confidence::saturating_from_basis_points(confidence),
    }
}

fn policy_is_valid(policy: EventCampaignPolicy) -> bool {
    policy.minimum_audience > 0
        && policy.announcement_max_days_before > policy.reminder_days_before
        && u64::from(policy.reminder_days_before) * 24 > u64::from(policy.last_call_hours_before)
        && policy.last_call_hours_before > policy.day_of_hours_before
        && policy.day_of_hours_before > 0
        && policy.thank_you_hours_after > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O.5: the approval has to say who it reaches. Every phase but one knows
    /// the number from the decision snapshot.
    #[test]
    fn every_phase_names_its_audience() {
        let snapshot = EventCampaignSnapshot {
            event_id: EventId::from_uuid(uuid::Uuid::nil()),
            published: true,
            communication_enabled: true,
            starts_at: OffsetDateTime::UNIX_EPOCH,
            interested_fans: 90,
            paid_buyers: 30,
            attendees: 70,
            history: EventCampaignHistory::default(),
        };
        assert_eq!(
            EventCampaignPhase::InterestReminder.audience_size(&snapshot),
            Some(60)
        );
        assert_eq!(
            EventCampaignPhase::LastCall.audience_size(&snapshot),
            Some(60)
        );
        assert_eq!(EventCampaignPhase::DayOf.audience_size(&snapshot), Some(30));
        assert_eq!(
            EventCampaignPhase::ThankYou.audience_size(&snapshot),
            Some(70)
        );
        // The one honest absence: the announcement's audience is the city's
        // consented fans, which this snapshot does not carry. A zero here would
        // read as "nobody" and stop a campaign that has an audience.
        assert_eq!(
            EventCampaignPhase::Announcement.audience_size(&snapshot),
            None
        );

        for phase in [
            EventCampaignPhase::Announcement,
            EventCampaignPhase::InterestReminder,
            EventCampaignPhase::LastCall,
            EventCampaignPhase::DayOf,
            EventCampaignPhase::ThankYou,
        ] {
            assert!(
                phase.audience_basis().len() > 20,
                "{phase:?} describes its audience as a fragment"
            );
        }
    }

    /// More buyers than interested fans is possible — somebody bought without
    /// ever marking interest — and must not underflow into a huge number.
    #[test]
    fn more_buyers_than_interested_is_an_empty_reminder_not_a_wrapped_one() {
        let snapshot = EventCampaignSnapshot {
            event_id: EventId::from_uuid(uuid::Uuid::nil()),
            published: true,
            communication_enabled: true,
            starts_at: OffsetDateTime::UNIX_EPOCH,
            interested_fans: 5,
            paid_buyers: 40,
            attendees: 0,
            history: EventCampaignHistory::default(),
        };
        assert_eq!(
            EventCampaignPhase::LastCall.audience_size(&snapshot),
            Some(0)
        );
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn snapshot(starts_in: Duration) -> EventCampaignSnapshot {
        EventCampaignSnapshot {
            event_id: EventId::new(),
            published: true,
            communication_enabled: true,
            starts_at: now() + starts_in,
            interested_fans: 30,
            paid_buyers: 10,
            attendees: 12,
            history: EventCampaignHistory::default(),
        }
    }

    #[test]
    fn interest_reminder_targets_unconverted_interest() {
        // Interest reminders only fire after an announcement has gone out.
        // Without this guard the announcement would fire first (wider window).
        let mut data = snapshot(Duration::days(10));
        data.history.announcement_sent = true;
        assert!(matches!(
            evaluate_event_campaign(data, EventCampaignPolicy::default(), now()),
            EventCampaignDecision::Request {
                phase: EventCampaignPhase::InterestReminder,
                ..
            }
        ));
    }

    #[test]
    fn paid_buyers_suppress_sales_reminder_when_no_unconverted_audience() {
        let mut data = snapshot(Duration::days(10));
        data.history.announcement_sent = true;
        data.paid_buyers = data.interested_fans;
        assert_eq!(
            evaluate_event_campaign(data, EventCampaignPolicy::default(), now()),
            EventCampaignDecision::Hold(EventCampaignHoldReason::InsufficientAudience)
        );
    }

    #[test]
    fn thank_you_requires_attendance_evidence() {
        let mut data = snapshot(-Duration::hours(12));
        data.attendees = 0;
        assert_eq!(
            evaluate_event_campaign(data, EventCampaignPolicy::default(), now()),
            EventCampaignDecision::Hold(EventCampaignHoldReason::InsufficientAudience)
        );
    }

    #[test]
    fn announcement_fires_before_interest_reminder_when_not_yet_sent() {
        // An event within the reminder window but without an announcement
        // should get an announcement, not an interest reminder. Without the
        // ordering fix, the interest_reminder guard (InsufficientAudience)
        // would fire first and block the announcement forever.
        let mut data = snapshot(Duration::days(10));
        data.interested_fans = 1; // below minimum_audience for reminder
        data.paid_buyers = 0;
        assert!(matches!(
            evaluate_event_campaign(data, EventCampaignPolicy::default(), now()),
            EventCampaignDecision::Request {
                phase: EventCampaignPhase::Announcement,
                ..
            }
        ));
    }
}
