//! The measurement ledger — the plan's fifteen claims, each stated with the
//! number that answers it or the reason this build cannot produce one.
//!
//! The plan judges the first ninety days on rates, not totals, and a rate is
//! only stated when its denominator clears the floor. Below the floor the
//! counts are shown and the rate is not — a 0% invented from two observations
//! is a lie with a number's shape, and a claim this build cannot measure says
//! why rather than rendering a blank.

use serde::Serialize;
use time::OffsetDateTime;

/// Rates are measurable at n=20, totals are not (plan §4e-6).
pub const RATE_FLOOR: i64 = 20;
pub const WINDOW_DAYS: u32 = 90;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Measure {
    /// Denominator cleared the floor; the rate is stated.
    Rate {
        numerator: i64,
        denominator: i64,
        basis_points: u16,
    },
    /// Too few to state a rate — the counts are shown, the rate is not.
    BelowFloor {
        numerator: i64,
        denominator: i64,
        floor: i64,
    },
    /// A count with a unit, where the claim is not a ratio.
    Count { value: i64, unit: &'static str },
    /// A median duration in minutes over `n` observations.
    Minutes { median: f64, n: i64 },
    /// This build cannot measure the claim; the reason is the value.
    Unmeasured { reason: &'static str },
}

impl Measure {
    /// `denominator < RATE_FLOOR` → BelowFloor (denominator 0 included — never
    /// a division, never a fabricated 0%). numerator > denominator is a caller
    /// bug: debug_assert, and clamp basis points to 10_000 in release.
    pub fn rate(numerator: i64, denominator: i64) -> Self {
        Self::rate_with_floor(numerator, denominator, RATE_FLOOR)
    }

    /// `rate` with a caller-chosen floor. The per-show rows of `room_leak`
    /// are judged at floor 1 — one show is one observation, and the plan's
    /// own words are "per show".
    pub fn rate_with_floor(numerator: i64, denominator: i64, floor: i64) -> Self {
        if denominator < floor {
            return Self::BelowFloor {
                numerator,
                denominator,
                floor,
            };
        }
        debug_assert!(
            numerator <= denominator,
            "rate numerator {numerator} exceeds denominator {denominator}"
        );
        let basis_points = u16::try_from(
            numerator
                .saturating_mul(10_000)
                .clamp(0, denominator.saturating_mul(10_000))
                / denominator,
        )
        .unwrap_or(10_000);
        Self::Rate {
            numerator,
            denominator,
            basis_points,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Breakdown {
    pub label: String,
    pub measure: Measure,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Claim {
    pub key: &'static str,
    /// The plan's words for the claim.
    pub claim: &'static str,
    /// The plan's words for how it is measured.
    pub measured_as: &'static str,
    pub window_days: u32,
    pub measure: Measure,
    /// Per-channel / per-show rows where the claim is judged per unit.
    pub breakdown: Vec<Breakdown>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MeasurementLedger {
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    pub window_days: u32,
    pub rate_floor: i64,
    pub claims: Vec<Claim>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_at_exactly_the_floor_is_stated() {
        assert_eq!(
            Measure::rate(10, 20),
            Measure::Rate {
                numerator: 10,
                denominator: 20,
                basis_points: 5_000,
            }
        );
    }

    #[test]
    fn one_below_the_floor_shows_the_counts_not_a_rate() {
        assert_eq!(
            Measure::rate(9, 19),
            Measure::BelowFloor {
                numerator: 9,
                denominator: 19,
                floor: 20,
            }
        );
    }

    #[test]
    fn an_empty_denominator_is_below_floor_not_a_division() {
        assert_eq!(
            Measure::rate(0, 0),
            Measure::BelowFloor {
                numerator: 0,
                denominator: 0,
                floor: 20,
            }
        );
    }

    #[test]
    fn a_numerator_equal_to_the_denominator_is_ten_thousand_basis_points() {
        assert_eq!(
            Measure::rate(20, 20),
            Measure::Rate {
                numerator: 20,
                denominator: 20,
                basis_points: 10_000,
            }
        );
    }

    #[test]
    fn a_caller_floor_of_one_states_the_rate_from_a_single_observation() {
        assert_eq!(
            Measure::rate_with_floor(1, 1, 1),
            Measure::Rate {
                numerator: 1,
                denominator: 1,
                basis_points: 10_000,
            }
        );
        assert_eq!(
            Measure::rate_with_floor(0, 1, 1),
            Measure::Rate {
                numerator: 0,
                denominator: 1,
                basis_points: 0,
            }
        );
    }

    #[test]
    fn a_third_is_3333_basis_points_floored_not_rounded() {
        assert_eq!(
            Measure::rate(20, 60),
            Measure::Rate {
                numerator: 20,
                denominator: 60,
                basis_points: 3_333,
            }
        );
    }

    #[test]
    fn the_serde_tag_is_state_and_unmeasured_carries_its_reason() {
        let unmeasured = serde_json::to_value(Measure::Unmeasured {
            reason: "not on this build",
        })
        .expect("serializes");
        assert_eq!(unmeasured["state"], "unmeasured");
        assert_eq!(unmeasured["reason"], "not on this build");

        let rate = serde_json::to_value(Measure::Rate {
            numerator: 1,
            denominator: 20,
            basis_points: 500,
        })
        .expect("serializes");
        assert_eq!(rate["state"], "rate");
    }
}
