//! Calendar routing hygiene — detect impractical distances between
//! consecutive confirmed shows.
//!
//! Phase 19 candidate: "Two confirmed shows 600 km apart on consecutive days
//! is a fact the system can see and nobody enjoys discovering late. Phase 7's
//! cost model already knows what the drive costs."
//!
//! This module is pure detection: given a pair of confirmed shows with dates
//! and distances from home base, decide whether the routing between them is
//! impractical. The distance between two shows is not the sum of their
//! distances from home — it is a triangle, and without coordinates the best
//! available proxy is the sum of distances from home minus the shorter leg,
//! which is an upper bound. When coordinates are available (future), the
//! actual great-circle distance should be used instead.

use serde::{Deserialize, Serialize};
use time::Date;

/// A confirmed show with the facts the routing rule needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShowRoutingFact {
    pub event_id: uuid::Uuid,
    pub show_date: Date,
    /// One-way road distance from home base, if known.
    pub distance_km: Option<u32>,
}

/// Policy for the routing conflict detector.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct CalendarRoutingPolicy {
    /// Maximum km that can be driven between consecutive-day shows without
    /// raising a conflict. Default 400 km — roughly 4 hours of driving,
    /// which is the practical limit for load-in same day.
    pub max_consecutive_day_km: u32,
    /// Maximum km for shows 2 days apart. Default 800 km.
    pub max_two_day_gap_km: u32,
}

impl Default for CalendarRoutingPolicy {
    fn default() -> Self {
        Self {
            max_consecutive_day_km: 400,
            max_two_day_gap_km: 800,
        }
    }
}

/// The detector's verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalendarRoutingDecision {
    /// No conflict detected. Either the shows are close enough, far enough
    /// apart in time, or the distance is unknown.
    Ok,
    /// Two consecutive-day shows are too far apart to drive between.
    Conflict {
        earlier: ShowRoutingFact,
        later: ShowRoutingFact,
        estimated_km: u32,
        /// Which threshold was exceeded.
        threshold_km: u32,
    },
}

/// Detects a routing conflict between two confirmed shows.
///
/// The distance between two shows is estimated as the sum of their distances
/// from home base, minus the shorter leg. This is an upper bound on the
/// actual driving distance — if either distance is unknown, the rule cannot
/// speak and returns `Ok` rather than guessing.
#[must_use]
pub fn evaluate_routing_conflict(
    earlier: &ShowRoutingFact,
    later: &ShowRoutingFact,
    policy: CalendarRoutingPolicy,
) -> CalendarRoutingDecision {
    // Only check shows that are close in time.
    let gap_days = (later.show_date - earlier.show_date).whole_days();
    if gap_days <= 0 {
        // Same day or out of order — not a routing conflict, possibly a
        // double-booking but that is a different rule.
        return CalendarRoutingDecision::Ok;
    }

    let (Some(earlier_km), Some(later_km)) = (earlier.distance_km, later.distance_km) else {
        // Without distances, the rule cannot speak.
        return CalendarRoutingDecision::Ok;
    };

    // Upper bound on driving distance between the two venues: sum of distances
    // from home minus the shorter leg. If both shows are 200 km from home in
    // the same direction, the inter-show distance is ~0, not 400.
    let shorter = earlier_km.min(later_km);
    let estimated_km = earlier_km + later_km - shorter;

    let threshold = if gap_days == 1 {
        policy.max_consecutive_day_km
    } else if gap_days == 2 {
        policy.max_two_day_gap_km
    } else {
        // Three or more days apart is enough travel time for any distance
        // within the band's operating range.
        return CalendarRoutingDecision::Ok;
    };

    if estimated_km > threshold {
        CalendarRoutingDecision::Conflict {
            earlier: *earlier,
            later: *later,
            estimated_km,
            threshold_km: threshold,
        }
    } else {
        CalendarRoutingDecision::Ok
    }
}

/// Scans a sorted list of shows for routing conflicts. Returns the first
/// conflict found, or `Ok` if none. Shows should be sorted by date ascending.
#[must_use]
pub fn scan_routing_conflicts(
    shows: &[ShowRoutingFact],
    policy: CalendarRoutingPolicy,
) -> CalendarRoutingDecision {
    for window in shows.windows(2) {
        let Some((earlier, rest)) = window.split_first() else {
            continue;
        };
        let Some(later) = rest.first() else {
            continue;
        };
        let decision = evaluate_routing_conflict(earlier, later, policy);
        if matches!(decision, CalendarRoutingDecision::Conflict { .. }) {
            return decision;
        }
    }
    CalendarRoutingDecision::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn date(year: i32, month: Month, day: u8) -> Date {
        Date::from_calendar_date(year, month, day).unwrap()
    }

    fn show(date: Date, km: Option<u32>) -> ShowRoutingFact {
        ShowRoutingFact {
            event_id: uuid::Uuid::nil(),
            show_date: date,
            distance_km: km,
        }
    }

    #[test]
    fn consecutive_days_too_far_apart_conflicts() {
        let earlier = show(date(2026, Month::September, 1), Some(350));
        // Need estimated > 400: 350 + 450 - 350 = 450
        let later = show(date(2026, Month::September, 2), Some(450));
        let decision =
            evaluate_routing_conflict(&earlier, &later, CalendarRoutingPolicy::default());
        assert!(matches!(
            decision,
            CalendarRoutingDecision::Conflict {
                estimated_km: 450,
                threshold_km: 400,
                ..
            }
        ));
    }

    #[test]
    fn consecutive_days_close_enough_ok() {
        let earlier = show(date(2026, Month::September, 1), Some(100));
        let later = show(date(2026, Month::September, 2), Some(150));
        let decision =
            evaluate_routing_conflict(&earlier, &later, CalendarRoutingPolicy::default());
        assert_eq!(decision, CalendarRoutingDecision::Ok);
    }

    #[test]
    fn two_days_apart_uses_higher_threshold() {
        let earlier = show(date(2026, Month::September, 1), Some(400));
        let later = show(date(2026, Month::September, 3), Some(450));
        let decision =
            evaluate_routing_conflict(&earlier, &later, CalendarRoutingPolicy::default());
        // estimated: 400 + 450 - 400 = 450, threshold 800 → ok
        assert_eq!(decision, CalendarRoutingDecision::Ok);
    }

    #[test]
    fn two_days_apart_too_far_conflicts() {
        let earlier = show(date(2026, Month::September, 1), Some(500));
        let later = show(date(2026, Month::September, 3), Some(600));
        let decision =
            evaluate_routing_conflict(&earlier, &later, CalendarRoutingPolicy::default());
        // estimated: 500 + 600 - 500 = 600, threshold 800 → ok
        assert_eq!(decision, CalendarRoutingDecision::Ok);
    }

    #[test]
    fn three_days_apart_never_conflicts() {
        let earlier = show(date(2026, Month::September, 1), Some(1000));
        let later = show(date(2026, Month::September, 4), Some(1000));
        let decision =
            evaluate_routing_conflict(&earlier, &later, CalendarRoutingPolicy::default());
        assert_eq!(decision, CalendarRoutingDecision::Ok);
    }

    #[test]
    fn missing_distance_ok() {
        let earlier = show(date(2026, Month::September, 1), None);
        let later = show(date(2026, Month::September, 2), Some(350));
        let decision =
            evaluate_routing_conflict(&earlier, &later, CalendarRoutingPolicy::default());
        assert_eq!(decision, CalendarRoutingDecision::Ok);
    }

    #[test]
    fn same_day_ok() {
        let earlier = show(date(2026, Month::September, 1), Some(500));
        let later = show(date(2026, Month::September, 1), Some(500));
        let decision =
            evaluate_routing_conflict(&earlier, &later, CalendarRoutingPolicy::default());
        assert_eq!(decision, CalendarRoutingDecision::Ok);
    }

    #[test]
    fn scan_finds_first_conflict() {
        let shows = vec![
            show(date(2026, Month::September, 1), Some(100)),
            show(date(2026, Month::September, 2), Some(100)),
            show(date(2026, Month::September, 3), Some(500)),
        ];
        let decision = scan_routing_conflicts(&shows, CalendarRoutingPolicy::default());
        // Sep 2 → Sep 3: 100 + 500 - 100 = 500 > 400 → conflict
        assert!(matches!(decision, CalendarRoutingDecision::Conflict { .. }));
    }

    #[test]
    fn scan_no_conflicts_ok() {
        let shows = vec![
            show(date(2026, Month::September, 1), Some(100)),
            show(date(2026, Month::September, 3), Some(200)),
            show(date(2026, Month::September, 7), Some(300)),
        ];
        let decision = scan_routing_conflicts(&shows, CalendarRoutingPolicy::default());
        assert_eq!(decision, CalendarRoutingDecision::Ok);
    }

    #[test]
    fn same_direction_shows_not_conflict() {
        // Both 150 km from home, same direction → inter-show distance ~0
        let earlier = show(date(2026, Month::September, 1), Some(150));
        let later = show(date(2026, Month::September, 2), Some(160));
        let decision =
            evaluate_routing_conflict(&earlier, &later, CalendarRoutingPolicy::default());
        // estimated: 150 + 160 - 150 = 160 < 400 → ok
        assert_eq!(decision, CalendarRoutingDecision::Ok);
    }
}
