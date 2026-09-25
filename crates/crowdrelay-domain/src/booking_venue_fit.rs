//! The cold-start booking lane: a venue's own booking history as demand
//! evidence the city fan-density score cannot see.
//!
//! A band with twenty-three fans will never clear the demand path's
//! `minimum_score = 65` — `active_fans` alone needs a hundred in one city —
//! so the honest question at cold start is not "do our fans fill it" but
//! "does this room already book acts like us". The `place_venue_marks`
//! graph answers that for venue-linked targets.
//!
//! Everything the gate keeps is about contact, not merit — the same
//! convention as the festival lane: reachable, not mid-conversation,
//! outside cooldown. `declined` deliberately does not hold: a booker's
//! no is about a date, and the 180-day cooldown plus the operator's
//! approval decide whether re-asking is honest.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::autonomy::Confidence;
use crate::booking::{BookingReplyDisposition, BookingTargetKind, BookingTargetSnapshot};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct VenueFitPolicy {
    /// The room must have hosted at least this many marked shows in the
    /// last year — a room nobody played is not a fit signal at all.
    pub minimum_shows_12m: i64,
    /// Distinct comparable-genre acts on the room's bills — one shared bill
    /// can be a coincidence; two is a booking pattern.
    pub minimum_comparable_acts: i64,
    /// Operator priority still gates the cold-start lane — a room the
    /// operator deprioritized is not rescued by its marks.
    pub minimum_priority: u16,
    /// The same contact cooldown the anchor selection enforces.
    pub target_cooldown_days: u32,
}

impl Default for VenueFitPolicy {
    fn default() -> Self {
        Self {
            minimum_shows_12m: 3,
            minimum_comparable_acts: 2,
            minimum_priority: 20,
            target_cooldown_days: 180,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VenueFitDecision {
    Hold(VenueFitHoldReason),
    /// The score is evidence-shaped — comparable bills plus recent shows —
    /// not the city score: this lane exists precisely because the city
    /// score cannot be reached yet.
    Request {
        confidence: Confidence,
        evidence_score: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VenueFitHoldReason {
    /// Festivals keep their own deadline-driven lane.
    Festival,
    /// No `venue_evidence` row — the room's history was never observed, and
    /// absent evidence is not zero evidence.
    NoVenueEvidence,
    /// The room does not demonstrably book acts like this band.
    WeakFit,
    /// The operator deprioritized this target.
    LowPriority,
    Inactive,
    NoBooking,
    /// Somebody has written and not yet heard back.
    LiveThread,
    DoNotContact,
    OutreachInFlight,
    CooldownActive,
}

/// The cold-start lane: a venue's own booking history is demand evidence
/// the city fan-density score cannot see. A band with twenty-three fans
/// will never clear `minimum_score = 65` — `active_fans` alone needs a
/// hundred in one city — so the honest question at cold start is not "do
/// our fans fill it" but "does this room already book acts like us". The
/// `place_venue_marks` graph answers that for venue-linked targets.
///
/// Everything the gate keeps is about contact, not merit — the same
/// convention as the festival lane: reachable, not mid-conversation,
/// outside cooldown. `declined` deliberately does not hold: a booker's
/// no is about a date, and the 180-day cooldown plus the operator's
/// approval decide whether re-asking is honest.
#[must_use]
pub fn evaluate_venue_fit(
    target: &BookingTargetSnapshot,
    policy: VenueFitPolicy,
    now: OffsetDateTime,
) -> VenueFitDecision {
    if target.kind == BookingTargetKind::Festival {
        return VenueFitDecision::Hold(VenueFitHoldReason::Festival);
    }
    if !target.active {
        return VenueFitDecision::Hold(VenueFitHoldReason::Inactive);
    }
    if !target.accepts_booking {
        return VenueFitDecision::Hold(VenueFitHoldReason::NoBooking);
    }
    if target.priority < policy.minimum_priority {
        return VenueFitDecision::Hold(VenueFitHoldReason::LowPriority);
    }
    if matches!(target.last_reply, BookingReplyDisposition::DoNotContact) {
        return VenueFitDecision::Hold(VenueFitHoldReason::DoNotContact);
    }
    if matches!(
        target.last_reply,
        BookingReplyDisposition::Received
            | BookingReplyDisposition::Positive
            | BookingReplyDisposition::Booked
    ) {
        return VenueFitDecision::Hold(VenueFitHoldReason::LiveThread);
    }
    if target.outreach_in_flight {
        return VenueFitDecision::Hold(VenueFitHoldReason::OutreachInFlight);
    }
    if target.last_outreach_at.is_some_and(|at| {
        at > now || now - at < Duration::days(i64::from(policy.target_cooldown_days))
    }) {
        return VenueFitDecision::Hold(VenueFitHoldReason::CooldownActive);
    }
    let Some(evidence) = &target.venue_evidence else {
        return VenueFitDecision::Hold(VenueFitHoldReason::NoVenueEvidence);
    };
    if evidence.shows_last_12m < policy.minimum_shows_12m
        || evidence.comparable_acts < policy.minimum_comparable_acts
    {
        return VenueFitDecision::Hold(VenueFitHoldReason::WeakFit);
    }
    // Evidence score: comparable bills weigh double — a room that once
    // hosted a similar act proves less than a room that keeps hosting them.
    let evidence_score = (evidence.shows_last_12m.min(10) as u16)
        .saturating_mul(5)
        .saturating_add((evidence.comparable_acts.min(5) as u16).saturating_mul(10))
        .min(100);
    // Confidence deliberately sits below the demand path's 7_500 base —
    // evidence of fit is real, and it is not evidence of our draw.
    VenueFitDecision::Request {
        confidence: Confidence::saturating_from_basis_points(
            6_000_u16.saturating_add((evidence.comparable_acts.min(5) as u16).saturating_mul(100)),
        ),
        evidence_score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::booking::BookingVenueEvidence;
    use crate::{BookingTargetId, CityId};

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn target(city_id: CityId, priority: u16, relationship_score: u16) -> BookingTargetSnapshot {
        BookingTargetSnapshot {
            target_id: BookingTargetId::new(),
            city_id,
            kind: BookingTargetKind::Venue,
            display_name: "Example Venue".to_owned(),
            capacity: None,
            version: 1,
            active: true,
            accepts_booking: true,
            priority,
            relationship_score,
            outreach_in_flight: false,
            last_outreach_at: None,
            followup_count: 0,
            last_reply: BookingReplyDisposition::None,
            venue_evidence: None,
            days_until_application_close: None,
            next_application_closes_at: None,
            linked_venue_ids: Vec::new(),
        }
    }

    fn fit_target(city: CityId, shows: i64, comparable: i64) -> BookingTargetSnapshot {
        let mut venue = target(city, 80, 50);
        venue.venue_evidence = Some(BookingVenueEvidence {
            shows_last_12m: shows,
            comparable_acts: comparable,
            genres: None,
            capacity: None,
            days_since_last_event: Some(12),
            booking_contact_days: None,
        });
        venue
    }

    #[test]
    fn a_room_that_books_comparable_acts_proposes_the_cold_start_ask() {
        let venue = fit_target(CityId::new(), 6, 3);
        assert!(matches!(
            evaluate_venue_fit(&venue, VenueFitPolicy::default(), now()),
            VenueFitDecision::Request { confidence, .. }
                if confidence.basis_points() < 7_500
        ));
    }

    #[test]
    fn a_room_with_no_observed_history_is_not_zero_evidence() {
        let venue = target(CityId::new(), 80, 50);
        assert_eq!(
            evaluate_venue_fit(&venue, VenueFitPolicy::default(), now()),
            VenueFitDecision::Hold(VenueFitHoldReason::NoVenueEvidence)
        );
    }

    #[test]
    fn a_room_that_does_not_book_like_us_holds() {
        for (shows, comparable) in [(2, 5), (8, 1), (0, 0)] {
            let venue = fit_target(CityId::new(), shows, comparable);
            assert_eq!(
                evaluate_venue_fit(&venue, VenueFitPolicy::default(), now()),
                VenueFitDecision::Hold(VenueFitHoldReason::WeakFit),
                "shows={shows} comparable={comparable} must hold"
            );
        }
    }

    #[test]
    fn a_festival_keeps_its_own_lane() {
        let mut venue = fit_target(CityId::new(), 10, 5);
        venue.kind = BookingTargetKind::Festival;
        assert_eq!(
            evaluate_venue_fit(&venue, VenueFitPolicy::default(), now()),
            VenueFitDecision::Hold(VenueFitHoldReason::Festival)
        );
    }

    #[test]
    fn a_live_thread_is_left_to_the_operator_in_the_fit_lane() {
        for disposition in [
            BookingReplyDisposition::Received,
            BookingReplyDisposition::Positive,
            BookingReplyDisposition::Booked,
        ] {
            let mut venue = fit_target(CityId::new(), 5, 4);
            venue.last_reply = disposition;
            assert_eq!(
                evaluate_venue_fit(&venue, VenueFitPolicy::default(), now()),
                VenueFitDecision::Hold(VenueFitHoldReason::LiveThread),
                "{disposition:?} must hold"
            );
        }
    }

    #[test]
    fn a_recent_letter_cools_the_fit_lane_too() {
        let mut venue = fit_target(CityId::new(), 5, 4);
        venue.last_outreach_at = Some(now() - Duration::days(30));
        assert_eq!(
            evaluate_venue_fit(&venue, VenueFitPolicy::default(), now()),
            VenueFitDecision::Hold(VenueFitHoldReason::CooldownActive)
        );
    }

    #[test]
    fn an_operator_deprioritized_room_is_not_rescued_by_its_marks() {
        let mut venue = fit_target(CityId::new(), 9, 6);
        venue.priority = 10;
        assert_eq!(
            evaluate_venue_fit(&venue, VenueFitPolicy::default(), now()),
            VenueFitDecision::Hold(VenueFitHoldReason::LowPriority)
        );
    }

    #[test]
    fn a_decline_does_not_close_the_room_forever() {
        let mut venue = fit_target(CityId::new(), 5, 4);
        venue.last_reply = BookingReplyDisposition::Declined;
        venue.last_outreach_at = Some(now() - Duration::days(200));
        assert!(matches!(
            evaluate_venue_fit(&venue, VenueFitPolicy::default(), now()),
            VenueFitDecision::Request { .. }
        ));
    }
}
