//! The proposed booking window — the "date" of a booking proposal (§12-6).
//!
//! A booking outreach that asks "are you free sometime?" gets the silence it
//! deserves. This module derives the concrete window the proposal asks for
//! from two deterministic inputs: the room's own event history (its rhythm —
//! how often it runs shows and how far ahead it books) and the band's own
//! calendar (shows the window must not collide with, and confirmed nights
//! close enough to make the ask a routing stop rather than a trip).
//!
//! The refusal is part of the design: a room with no observed history and no
//! show of ours anywhere near it yields `None` — no window is proposed, and
//! the outreach carries the room alone. A window with no basis is not
//! proposed; that refusal is the feature, because a date invented from
//! nothing reads exactly like what it is.
//!
//! Everything here is pure — inputs arrive already loaded, coordinates arrive
//! as `Option`, and a missing coordinate stays missing: no distance is ever
//! invented to make an `AdjacentShow` fire.

use serde::{Deserialize, Serialize};
use time::{Date, Duration, OffsetDateTime};

use crate::BookingTargetId;

/// Fallback lead time when the room's history is too thin to say how far
/// ahead it books — three weeks is the slow end of a small-room ask.
const FALLBACK_LEAD_DAYS: i64 = 21;
/// Fallback window span when no room rhythm exists.
const FALLBACK_SPAN_DAYS: i64 = 30;
/// A proposed start never sits inside this many days of an own show.
const COLLISION_GUARD_DAYS: i64 = 3;
/// The observed rhythm is trusted only inside this span — a room that runs
/// weekly does not get a year-long window, and one that runs twice a year
/// still gets a usable ask.
const MIN_SPAN_DAYS: i64 = 14;
const MAX_SPAN_DAYS: i64 = 90;
/// Roughly four hours of driving — the practical limit for a routing stop.
const ROUTING_DISTANCE_KM: f64 = 400.0;
/// An own show inside this many days of the window is an adjacent night.
const ADJACENT_WINDOW_DAYS: i64 = 5;
/// Fewer room shows than this cannot establish a median anything — one data
/// point is an anecdote, not a rhythm.
const MIN_ROOM_SAMPLE: usize = 2;

/// One of the tenant's own shows, as the calendar sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct BookingWindowOwnShow {
    pub starts_at: OffsetDateTime,
    /// The event's slug — carried so an `AdjacentShow` basis can name the
    /// night rather than describe it by date.
    pub slug: String,
    /// (latitude, longitude) of the show's city when known. `None` stays
    /// `None` — an unknown distance is never estimated into existence.
    pub coords: Option<(f64, f64)>,
    /// `true` for a published show. Adjacency is claimed only for confirmed
    /// nights — a draft still blocks the calendar but never anchors a
    /// routing argument to a promoter.
    pub confirmed: bool,
}

/// The per-target slice of the window load: the room's own history and where
/// it sits, when it sits anywhere known.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BookingWindowTargetInputs {
    pub target_id: BookingTargetId,
    /// (starts_at, created_at) of past shows marked at the room, newest
    /// first. `created_at` is the booking lag half of the lead-time read.
    pub room_shows: Vec<(OffsetDateTime, OffsetDateTime)>,
    /// The room's coordinates when the venue's city carries them.
    pub venue_coords: Option<(f64, f64)>,
}

/// Everything the window proposal needs, loaded once per evaluation cycle:
/// the tenant's own calendar is shared across every candidate target, and
/// each venue-linked target carries its own room history.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BookingWindowInputSet {
    /// The workspace's own published/draft shows — shared, loaded once.
    pub own_shows: Vec<BookingWindowOwnShow>,
    /// Per-target room history, only for targets that carry a `venue_id`.
    pub targets: Vec<BookingWindowTargetInputs>,
}

/// The one target's view of the inputs — what [`propose_booking_window`]
/// actually reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BookingWindowInputs {
    /// (starts_at, created_at) of past shows at the room, newest first.
    pub room_shows: Vec<(OffsetDateTime, OffsetDateTime)>,
    /// Own workspace's published/draft shows — the calendar the window must
    /// not collide with.
    pub own_shows: Vec<BookingWindowOwnShow>,
    /// The room's coordinates, when the venue row's city carries them.
    pub venue_coords: Option<(f64, f64)>,
}

/// The concrete ask — "the week of …" — plus the factors that produced it.
/// `basis` is the auditable half: every factor that actually contributed is
/// named, so the operator approving the send can see why these dates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProposedWindow {
    #[serde(with = "crate::wire_time::date")]
    pub start: Date,
    #[serde(with = "crate::wire_time::date")]
    pub end: Date,
    pub basis: Vec<WindowBasis>,
}

/// One factor that contributed to the window, typed so tests assert the
/// reason rather than the arithmetic.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowBasis {
    /// Median gap between the room's observed events — set the span.
    RoomRhythm { median_gap_days: i64 },
    /// Median created→starts lag on the room's events — how far out to ask.
    LeadTime { median_days: i64 },
    /// The band's own calendar — first clear day after a colliding show.
    CalendarClear { after: Date },
    /// A confirmed own show within routing distance — the adjacent night.
    AdjacentShow {
        event_slug: String,
        distance_km: i64,
    },
}

/// Derives the proposed window, or refuses when nothing supports one.
///
/// `start` is `now + median lead time` when at least two room shows supply a
/// lag, else `now + 21 days` — then walked forward to the first day that is
/// not within ±3 days of an own show. `end` is `start + median gap` clamped
/// to 14–90 days when the room's rhythm is known, else `start + 30 days`.
#[must_use]
pub fn propose_booking_window(
    evidence_series: &BookingWindowInputs,
    now: OffsetDateTime,
) -> Option<ProposedWindow> {
    // The room's own confirmed shows within routing distance — the basis an
    // empty room history needs for a proposal to exist at all. Missing
    // coordinates on either side mean "not within distance", never a guess.
    let nearby: Vec<(&BookingWindowOwnShow, i64)> = evidence_series
        .venue_coords
        .map(|venue| {
            evidence_series
                .own_shows
                .iter()
                .filter(|show| show.confirmed)
                .filter_map(|show| {
                    show.coords
                        .map(|coords| (show, haversine_km(coords, venue)))
                })
                .filter(|(_, km)| *km <= ROUTING_DISTANCE_KM)
                .map(|(show, km)| (show, km.round() as i64))
                .collect()
        })
        .unwrap_or_default();
    // A window with no basis is not proposed: no room rhythm to ride and no
    // night of ours anywhere near to route through.
    if evidence_series.room_shows.is_empty() && nearby.is_empty() {
        return None;
    }

    let mut basis = Vec::new();

    // Median created→starts lag — how far ahead this room actually books.
    // Fewer than two observations is an anecdote and falls back to 21 days.
    let median_lead_days = if evidence_series.room_shows.len() >= MIN_ROOM_SAMPLE {
        let mut lags: Vec<i64> = evidence_series
            .room_shows
            .iter()
            .map(|(starts_at, created_at)| (*starts_at - *created_at).whole_days().max(0))
            .collect();
        Some(median_days(&mut lags))
    } else {
        None
    };
    if let Some(median_days) = median_lead_days {
        basis.push(WindowBasis::LeadTime { median_days });
    }

    let mut start = now.date() + Duration::days(median_lead_days.unwrap_or(FALLBACK_LEAD_DAYS));

    // Walk the start forward until it clears every own show by ±3 days. Each
    // pass moves past the earliest still-colliding show, so the loop is
    // bounded by the number of own shows — it can never revisit a show it
    // already cleared.
    let mut cleared_after: Option<Date> = None;
    for _ in 0..=evidence_series.own_shows.len() {
        let colliding = evidence_series
            .own_shows
            .iter()
            .map(|show| show.starts_at.date())
            .filter(|show_date| (start - *show_date).whole_days().abs() <= COLLISION_GUARD_DAYS)
            .min();
        let Some(show_date) = colliding else {
            break;
        };
        cleared_after = Some(show_date);
        start = show_date + Duration::days(COLLISION_GUARD_DAYS + 1);
    }
    if let Some(after) = cleared_after {
        basis.push(WindowBasis::CalendarClear { after });
    }

    // Median gap between the room's observed shows — the rhythm that sets
    // the span, clamped so neither a weekly room nor a twice-a-year one
    // breaks the ask.
    let median_gap_days = if evidence_series.room_shows.len() >= MIN_ROOM_SAMPLE {
        let mut starts: Vec<OffsetDateTime> = evidence_series
            .room_shows
            .iter()
            .map(|(starts_at, _)| *starts_at)
            .collect();
        starts.sort_unstable();
        let mut gaps: Vec<i64> = starts
            .windows(2)
            .filter_map(|pair| match pair {
                [earlier, later] => Some((*later - *earlier).whole_days()),
                _ => None,
            })
            .collect();
        Some(median_days(&mut gaps))
    } else {
        None
    };
    let span_days = match median_gap_days {
        Some(gap) => {
            basis.push(WindowBasis::RoomRhythm {
                median_gap_days: gap,
            });
            gap.clamp(MIN_SPAN_DAYS, MAX_SPAN_DAYS)
        }
        None => FALLBACK_SPAN_DAYS,
    };
    let end = start + Duration::days(span_days);

    // A confirmed show of ours within 400 km of the room and ±5 days of the
    // window is the routing argument — "we are already driving past". Each
    // one that qualifies is named; nearest first, slug as the final order.
    let mut adjacent: Vec<(&BookingWindowOwnShow, i64)> = nearby
        .into_iter()
        .filter(|(show, _)| {
            let show_date = show.starts_at.date();
            (show_date - start).whole_days() >= -ADJACENT_WINDOW_DAYS
                && (show_date - end).whole_days() <= ADJACENT_WINDOW_DAYS
        })
        .collect();
    adjacent.sort_by(|(a, a_km), (b, b_km)| a_km.cmp(b_km).then_with(|| a.slug.cmp(&b.slug)));
    for (show, distance_km) in adjacent {
        basis.push(WindowBasis::AdjacentShow {
            event_slug: show.slug.clone(),
            distance_km,
        });
    }

    Some(ProposedWindow { start, end, basis })
}

/// Median of a non-empty day-count series; for an even count the two middle
/// values are averaged so the answer stays on the observations, not on a
/// coin flip.
fn median_days(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values.get(mid).copied().unwrap_or_default()
    } else {
        match (values.get(mid - 1).copied(), values.get(mid).copied()) {
            (Some(lower), Some(upper)) => (lower + upper) / 2,
            _ => 0,
        }
    }
}

/// Great-circle distance in kilometres. Missing coordinates never reach this
/// function — `None` is filtered before the call, so the answer is always a
/// measured distance, not an assumed one.
fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6_371.0;
    let to_radians = |degrees: f64| degrees * std::f64::consts::PI / 180.0;
    let delta_lat = to_radians(b.0 - a.0);
    let delta_lng = to_radians(b.1 - a.1);
    let h = (delta_lat / 2.0).sin().powi(2)
        + to_radians(a.0).cos() * to_radians(b.0).cos() * (delta_lng / 2.0).sin().powi(2);
    EARTH_RADIUS_KM * 2.0 * h.sqrt().atan2((1.0 - h).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn own_show(
        days_ahead: i64,
        slug: &str,
        coords: Option<(f64, f64)>,
        confirmed: bool,
    ) -> BookingWindowOwnShow {
        BookingWindowOwnShow {
            starts_at: now() + Duration::days(days_ahead),
            slug: slug.to_owned(),
            coords,
            confirmed,
        }
    }

    /// A room show observed `days_ago`, booked `lag_days` before it ran.
    fn room_show(days_ago: i64, lag_days: i64) -> (OffsetDateTime, OffsetDateTime) {
        let starts_at = now() - Duration::days(days_ago);
        (starts_at, starts_at - Duration::days(lag_days))
    }

    #[test]
    fn empty_evidence_proposes_nothing() {
        let inputs = BookingWindowInputs::default();
        assert_eq!(propose_booking_window(&inputs, now()), None);
    }

    #[test]
    fn no_room_history_and_no_nearby_show_proposes_nothing() {
        // A distant own show beyond routing distance is still no basis.
        let inputs = BookingWindowInputs {
            room_shows: Vec::new(),
            own_shows: vec![own_show(40, "far-show", Some((70.0, 20.0)), true)],
            venue_coords: Some((52.23, 21.01)), // Warsaw
        };
        assert_eq!(propose_booking_window(&inputs, now()), None);
    }

    #[test]
    fn rhythm_only_input_lands_at_the_lead_time_distance() {
        // Three shows 28 days apart, each booked ~42 days ahead.
        let inputs = BookingWindowInputs {
            room_shows: vec![room_show(20, 42), room_show(48, 40), room_show(76, 44)],
            own_shows: Vec::new(),
            venue_coords: None,
        };
        let window = propose_booking_window(&inputs, now()).expect("a window");
        let today = now().date();
        assert_eq!(window.start, today + Duration::days(42));
        assert_eq!(window.end, window.start + Duration::days(28));
        assert!(
            window
                .basis
                .contains(&WindowBasis::LeadTime { median_days: 42 })
        );
        assert!(window.basis.contains(&WindowBasis::RoomRhythm {
            median_gap_days: 28
        }));
        // Nothing collided and nothing is adjacent — no borrowed basis.
        assert_eq!(window.basis.len(), 2);
    }

    #[test]
    fn thin_room_history_falls_back_to_twenty_one_and_thirty() {
        let inputs = BookingWindowInputs {
            room_shows: vec![room_show(10, 60)],
            own_shows: Vec::new(),
            venue_coords: None,
        };
        let window = propose_booking_window(&inputs, now()).expect("a window");
        let today = now().date();
        assert_eq!(window.start, today + Duration::days(21));
        assert_eq!(window.end, window.start + Duration::days(30));
        assert_eq!(window.basis.len(), 0);
    }

    #[test]
    fn an_own_show_week_moves_the_start_past_it() {
        let inputs = BookingWindowInputs {
            room_shows: vec![room_show(20, 42), room_show(48, 42)],
            // The default 42-day-out start collides with a show at +40.
            own_shows: vec![own_show(40, "own-show", None, true)],
            venue_coords: None,
        };
        let window = propose_booking_window(&inputs, now()).expect("a window");
        // +40's show blocks [+37, +43]; the first clear day is +44.
        let today = now().date();
        assert_eq!(window.start, today + Duration::days(44));
        assert!(window.basis.contains(&WindowBasis::CalendarClear {
            after: today + Duration::days(40),
        }));
    }

    #[test]
    fn a_confirmed_show_within_routing_distance_names_the_adjacent_night() {
        // Warsaw venue; Kraków show (≈250 km) inside the window ±5 days.
        let inputs = BookingWindowInputs {
            room_shows: vec![room_show(20, 42), room_show(48, 42)],
            own_shows: vec![
                own_show(45, "krakow-night", Some((50.06, 19.94)), true),
                // Same distance but a draft — adjacency is only claimed for
                // confirmed nights.
                own_show(46, "draft-night", Some((50.06, 19.94)), false),
            ],
            venue_coords: Some((52.23, 21.01)),
        };
        let window = propose_booking_window(&inputs, now()).expect("a window");
        let adjacent: Vec<i64> = window
            .basis
            .iter()
            .filter_map(|basis| match basis {
                WindowBasis::AdjacentShow {
                    event_slug,
                    distance_km,
                } => {
                    assert_eq!(event_slug, "krakow-night");
                    Some(*distance_km)
                }
                _ => None,
            })
            .collect();
        assert_eq!(adjacent.len(), 1);
        assert!((240..=270).contains(&adjacent[0]));
    }

    #[test]
    fn a_nearby_show_alone_is_enough_basis_to_propose() {
        let inputs = BookingWindowInputs {
            room_shows: Vec::new(),
            // +26 clears the +21 fallback start (5 days out, not within the
            // ±3-day collision guard) and still lands inside the window ±5.
            own_shows: vec![own_show(26, "krakow-night", Some((50.06, 19.94)), true)],
            venue_coords: Some((52.23, 21.01)),
        };
        let window = propose_booking_window(&inputs, now()).expect("a window");
        let today = now().date();
        // Fallback lead time: +21; the show at +26 sits within ±5 days of
        // the [+21, +51] window.
        assert_eq!(window.start, today + Duration::days(21));
        assert!(window.basis.iter().any(|basis| matches!(
            basis,
            WindowBasis::AdjacentShow { event_slug, .. } if event_slug == "krakow-night"
        )));
    }

    #[test]
    fn missing_venue_coordinates_never_invent_a_distance() {
        let inputs = BookingWindowInputs {
            room_shows: Vec::new(),
            own_shows: vec![own_show(22, "krakow-night", Some((50.06, 19.94)), true)],
            venue_coords: None,
        };
        // No room history and the venue's location is unknown — the show is
        // not "within routing distance" of a point that does not exist.
        assert_eq!(propose_booking_window(&inputs, now()), None);
    }

    #[test]
    fn proposed_window_serde_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let window = ProposedWindow {
            start: now().date() + Duration::days(21),
            end: now().date() + Duration::days(51),
            basis: vec![
                WindowBasis::LeadTime { median_days: 35 },
                WindowBasis::RoomRhythm {
                    median_gap_days: 28,
                },
                WindowBasis::CalendarClear {
                    after: now().date() + Duration::days(19),
                },
                WindowBasis::AdjacentShow {
                    event_slug: "krakow-night".to_owned(),
                    distance_km: 252,
                },
            ],
        };
        let json = serde_json::to_value(&window)?;
        let back: ProposedWindow = serde_json::from_value(json)?;
        assert_eq!(back, window);
        Ok(())
    }
}
