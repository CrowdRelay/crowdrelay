//! Place: where organising is worth the weekend of handwork.
//!
//! The city funnel already answers each question separately — how many
//! fans a city holds, whether it is growing, how many a show there would
//! actually page, what rooms and promoters exist, and when we last
//! played. `organise_score` is the deterministic ranker over exactly
//! those inputs: the answer to "which city do we organise in next",
//! with every input still visible in the row so the ranking stays
//! explainable instead of trusted.
//!
//! A city with a show already booked is not a gap — `next_show_at` set
//! scores zero, whatever the rest says. The operator's question is where
//! a weekend produces something new, not where to feel good about a show
//! that is already coming.

/// Inputs for the organise-now score — mirrors the city-funnel row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrganiseCityInputs {
    /// Fans the nearby-gig emitter would page for a show here.
    pub reachable: u32,
    /// All fans who declared interest in the city.
    pub fans: u32,
    /// Fans who arrived inside the last 30 days.
    pub new_30d: u32,
    /// Confirmed bookable targets by kind.
    pub venues: u32,
    pub promoters: u32,
    pub festivals: u32,
    /// Whole months since the last published or completed show in the
    /// city; `None` means never on record — the deepest gap there is.
    pub months_since_show: Option<u32>,
    /// A show already booked forward closes the gap regardless of
    /// everything else.
    pub next_show_booked: bool,
}

/// The score in basis points, `0..=10_000` — same convention the
/// opportunity evaluators use, so thresholds read the same everywhere.
#[must_use]
pub fn organise_score(inputs: &OrganiseCityInputs) -> u16 {
    if inputs.next_show_booked {
        return 0;
    }
    // Reachable is the money metric: a show in a city pages exactly these
    // fans. 200 caps — the campaign plan's own "two hundred in four cities
    // produce four shows" floor. Declared-but-unreachable interest still
    // fills rooms through channels, at quarter weight.
    let reachable_bp = i64::from(inputs.reachable.min(200)) * 25;
    let declared_bp = i64::from(inputs.fans.min(400)) * 5;
    // Staleness: eighteen months is the plan's own gap horizon; deeper
    // than that (including never) does not keep scoring or every
    // never-played city would outrank every real one.
    let gap_months = inputs.months_since_show.unwrap_or(18).min(18);
    let gap_bp = i64::from(gap_months) * 140;
    // Supply: a confirmed route is what turns a weekend into a booking.
    // A venue is the direct thing; a promoter opens many rooms; a
    // festival is one shot at many fans. Capped — inventory beyond two
    // routes changes convenience, not feasibility.
    let supply_bp = (i64::from(inputs.venues.min(2)) * 1_200
        + i64::from(inputs.promoters.min(2)) * 700
        + i64::from(inputs.festivals.min(2)) * 1_000)
        .min(2_000);
    // Momentum: a city adding fans now is a city whose show lands
    // different from last year's. Bounded so one viral month cannot
    // outrank a room-sized audience.
    let momentum_bp = i64::from(inputs.new_30d.min(50)) * 10;

    (reachable_bp + declared_bp + gap_bp + supply_bp + momentum_bp).min(10_000) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_booked_show_is_not_a_gap() {
        let score = organise_score(&OrganiseCityInputs {
            reachable: 300,
            fans: 500,
            new_30d: 50,
            venues: 3,
            promoters: 1,
            festivals: 1,
            months_since_show: Some(30),
            next_show_booked: true,
        });
        assert_eq!(score, 0);
    }

    #[test]
    fn reachable_fans_outrank_declared_only() {
        let reachable_city = organise_score(&OrganiseCityInputs {
            reachable: 120,
            fans: 150,
            new_30d: 0,
            venues: 0,
            promoters: 0,
            festivals: 0,
            months_since_show: None,
            next_show_booked: false,
        });
        let declared_only = organise_score(&OrganiseCityInputs {
            reachable: 0,
            fans: 400,
            new_30d: 0,
            venues: 0,
            promoters: 0,
            festivals: 0,
            months_since_show: None,
            next_show_booked: false,
        });
        assert!(reachable_city > declared_only);
    }

    #[test]
    fn a_city_with_no_room_scores_below_one_with_supply() {
        let bare = organise_score(&OrganiseCityInputs {
            reachable: 60,
            fans: 80,
            new_30d: 5,
            venues: 0,
            promoters: 0,
            festivals: 0,
            months_since_show: Some(12),
            next_show_booked: false,
        });
        let supplied = organise_score(&OrganiseCityInputs {
            venues: 1,
            ..OrganiseCityInputs {
                reachable: 60,
                fans: 80,
                new_30d: 5,
                venues: 0,
                promoters: 0,
                festivals: 0,
                months_since_show: Some(12),
                next_show_booked: false,
            }
        });
        assert_eq!(supplied - bare, 1_200);
    }

    #[test]
    fn never_played_and_eighteen_months_score_the_same_gap() {
        let never = organise_score(&OrganiseCityInputs {
            reachable: 10,
            fans: 10,
            new_30d: 0,
            venues: 0,
            promoters: 0,
            festivals: 0,
            months_since_show: None,
            next_show_booked: false,
        });
        let ancient = organise_score(&OrganiseCityInputs {
            months_since_show: Some(40),
            ..OrganiseCityInputs {
                reachable: 10,
                fans: 10,
                new_30d: 0,
                venues: 0,
                promoters: 0,
                festivals: 0,
                months_since_show: None,
                next_show_booked: false,
            }
        });
        assert_eq!(never, ancient);
    }

    #[test]
    fn score_never_exceeds_ten_thousand() {
        let maxed = organise_score(&OrganiseCityInputs {
            reachable: 10_000,
            fans: 10_000,
            new_30d: 10_000,
            venues: 50,
            promoters: 50,
            festivals: 50,
            months_since_show: None,
            next_show_booked: false,
        });
        assert_eq!(maxed, 10_000);
    }
}
