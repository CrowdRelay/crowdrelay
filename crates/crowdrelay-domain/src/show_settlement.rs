//! What a show was predicted to cost, what it actually cost, and how wrong the
//! model was.
//!
//! Phase 7 gave the band a cost model good enough to refuse a gig with. Nothing
//! ever checked it against reality, so a rate that was wrong stayed wrong and
//! kept refusing — or accepting — shows on a number nobody had tested.
//!
//! Three rules make the comparison worth making.
//!
//! 1. **The prediction is frozen before the show, not reconstructed after it.**
//!    Recomputing an estimate at settlement time uses today's rates, which is a
//!    comparison of today's model against itself.
//! 2. **A settlement without a prediction is refused.** There is no honest way
//!    to score a model against a show it was never asked about.
//! 3. **The verdict is about the model, never about the show.** A gig that lost
//!    money because the van broke is not evidence that the rates are wrong. What
//!    this module reports is how far each line was out and which line was worst,
//!    so an operator changes a number rather than a habit.

use serde::{Deserialize, Serialize};

use crate::tour_economics::ShowCost;

/// One line of the cost model, plus the line the model does not have.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostLine {
    Transport,
    Accommodation,
    PerDiem,
    Overhead,
    /// Real money the model has no line for. When this is the worst line, the
    /// finding is not that a rate is wrong — it is that the model is missing a
    /// cost the band actually pays.
    Unmodelled,
    /// What arrived against what was offered. A fee that shrinks between the
    /// offer and the bank is a different problem from a cost that grows.
    Fee,
}

impl CostLine {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Accommodation => "accommodation",
            Self::PerDiem => "per_diem",
            Self::Overhead => "overhead",
            Self::Unmodelled => "unmodelled",
            Self::Fee => "fee",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "transport" => Some(Self::Transport),
            "accommodation" => Some(Self::Accommodation),
            "per_diem" => Some(Self::PerDiem),
            "overhead" => Some(Self::Overhead),
            "unmodelled" => Some(Self::Unmodelled),
            "fee" => Some(Self::Fee),
            _ => None,
        }
    }

    /// What an operator changes when this line is the one that is wrong.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::Transport => {
                "adjust the all-in road rate per 100 km, or the fuel price and consumption behind it"
            }
            Self::Accommodation => "adjust the room rate, or the overnight threshold",
            Self::PerDiem => "adjust the per-diem, or the crew size it is multiplied by",
            Self::Overhead => "adjust the fixed overhead per show",
            Self::Unmodelled => {
                "the band paid for something the model has no line for; decide whether it belongs \
                 in overhead or is a one-off"
            }
            Self::Fee => {
                "the fee that arrived differs from the fee that was offered; check the deal terms \
                 rather than the cost model"
            }
        }
    }
}

/// What a show actually cost, as somebody who was there reports it.
///
/// Every field is money that left or arrived. Nothing here is derived, because
/// a derived settlement is another estimate wearing a different name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SettledShowCost {
    pub transport_minor: i64,
    pub accommodation_minor: i64,
    pub per_diem_minor: i64,
    pub overhead_minor: i64,
    /// Money spent that no line above covers.
    pub other_minor: i64,
    /// What arrived, not what was offered.
    pub fee_received_minor: i64,
}

impl SettledShowCost {
    #[must_use]
    pub const fn total_cost_minor(self) -> i64 {
        self.transport_minor
            .saturating_add(self.accommodation_minor)
            .saturating_add(self.per_diem_minor)
            .saturating_add(self.overhead_minor)
            .saturating_add(self.other_minor)
    }

    /// What the band was left with. Negative means it paid to play.
    #[must_use]
    pub const fn net_margin_minor(self) -> i64 {
        self.fee_received_minor
            .saturating_sub(self.total_cost_minor())
    }
}

/// How far one line was out.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LineVariance {
    pub line: CostLine,
    pub predicted_minor: i64,
    pub settled_minor: i64,
    /// Settled against predicted. `None` when the model predicted nothing for
    /// this line: a percentage against zero is not a large number, it is an
    /// undefined one.
    pub variance_basis_points: Option<i32>,
}

impl LineVariance {
    #[must_use]
    pub const fn delta_minor(self) -> i64 {
        self.settled_minor.saturating_sub(self.predicted_minor)
    }
}

/// Why a show cannot be scored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementGap {
    /// No prediction was frozen before the show. Recomputing one now would
    /// score the model against itself.
    NoPrediction,
    /// The prediction itself was an honest refusal — an input was missing, so
    /// there is no number to be wrong.
    PredictionIncomplete,
    /// Nobody has reported what the show cost.
    NoSettlement,
}

impl SettlementGap {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoPrediction => "no_prediction",
            Self::PredictionIncomplete => "prediction_incomplete",
            Self::NoSettlement => "no_settlement",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "no_prediction" => Some(Self::NoPrediction),
            "prediction_incomplete" => Some(Self::PredictionIncomplete),
            "no_settlement" => Some(Self::NoSettlement),
            _ => None,
        }
    }
}

/// The verdict on the model, not on the show.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "evidence")]
pub enum ModelAccuracy {
    /// Prediction and settlement agree inside the tolerance band.
    Calibrated {
        total_variance_basis_points: i32,
    },
    /// The model was materially wrong, and this line was the largest part of it.
    Drifting {
        total_variance_basis_points: i32,
        worst_line: CostLine,
        worst_line_delta_minor: i64,
    },
    Insufficient {
        reason: SettlementGap,
    },
}

impl ModelAccuracy {
    #[must_use]
    pub const fn worst_line(self) -> Option<CostLine> {
        match self {
            Self::Drifting { worst_line, .. } => Some(worst_line),
            Self::Calibrated { .. } | Self::Insufficient { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SettlementPolicy {
    /// Total cost may be out by this much before the model is called drifting.
    /// A band's costs are not a spreadsheet; one show inside a tenth is a model
    /// working, not a model to be adjusted.
    pub tolerance_basis_points: u16,
    /// Absolute money below which a variance is not worth an operator's
    /// attention, whatever the percentage says. Guards the same failure as the
    /// growth measurement's movement floor: a line predicted at nothing and
    /// settled at a few złoty is not a broken model.
    pub minimum_material_minor: i64,
}

impl Default for SettlementPolicy {
    fn default() -> Self {
        Self {
            tolerance_basis_points: 1_500,
            // Fifty złoty, in grosze.
            minimum_material_minor: 5_000,
        }
    }
}

/// Every line, predicted against settled.
///
/// Returned whole rather than only the worst one, because the point of an
/// itemised model is that an operator can see which number to change.
///
/// The offered fee is passed separately because `ShowCost` does not carry it:
/// the model costs a trip, and what was offered for it is an input frozen
/// alongside the estimate.
#[must_use]
pub fn line_variances(
    predicted: ShowCost,
    predicted_fee_minor: i64,
    settled: SettledShowCost,
) -> [LineVariance; 6] {
    let line = |line: CostLine, predicted_minor: i64, settled_minor: i64| LineVariance {
        line,
        predicted_minor,
        settled_minor,
        variance_basis_points: relative_variance(predicted_minor, settled_minor),
    };
    [
        line(
            CostLine::Transport,
            predicted.transport_minor,
            settled.transport_minor,
        ),
        line(
            CostLine::Accommodation,
            predicted.accommodation_minor,
            settled.accommodation_minor,
        ),
        line(
            CostLine::PerDiem,
            predicted.per_diem_minor,
            settled.per_diem_minor,
        ),
        line(
            CostLine::Overhead,
            predicted.overhead_minor,
            settled.overhead_minor,
        ),
        // The model predicts nothing here by construction, so the variance is
        // always undefined and the delta is the whole finding.
        line(CostLine::Unmodelled, 0, settled.other_minor),
        line(
            CostLine::Fee,
            predicted_fee_minor,
            settled.fee_received_minor,
        ),
    ]
}

/// Scores the model against one settled show.
#[must_use]
pub fn assess_model_accuracy(
    predicted: ShowCost,
    predicted_fee_minor: i64,
    settled: SettledShowCost,
    policy: SettlementPolicy,
) -> ModelAccuracy {
    let total_variance_basis_points =
        relative_variance(predicted.total_cost_minor, settled.total_cost_minor()).unwrap_or(0);
    let variances = line_variances(predicted, predicted_fee_minor, settled);
    // The worst line is the one that moved the most money, not the one with the
    // largest percentage. A per-diem out by 200% is worth less of an operator's
    // attention than transport out by 20% on ten times the money.
    let worst = variances
        .into_iter()
        // The fee is deliberately eligible: a fee that shrinks between the
        // offer and the bank is a real finding, and it is not a cost rate.
        .filter(|variance| {
            variance.delta_minor().unsigned_abs() >= policy.minimum_material_minor.unsigned_abs()
        })
        .max_by_key(|variance| variance.delta_minor().unsigned_abs());
    let material =
        total_variance_basis_points.unsigned_abs() > u32::from(policy.tolerance_basis_points);
    match worst {
        Some(worst) if material => ModelAccuracy::Drifting {
            total_variance_basis_points,
            worst_line: worst.line,
            worst_line_delta_minor: worst.delta_minor(),
        },
        // Either the total was inside tolerance, or no single line moved enough
        // money to name. Both mean the same thing to an operator: leave the
        // rates alone.
        _ => ModelAccuracy::Calibrated {
            total_variance_basis_points,
        },
    }
}

/// The road rate this show implies, in minor units per 100 km of round trip.
///
/// Offered as evidence, never applied. A rate is the operator's declaration of
/// what their own van costs, and one show is a data point rather than a policy.
#[must_use]
pub fn implied_transport_rate_minor_per_100km(
    settled_transport_minor: i64,
    round_trip_km: u32,
) -> Option<i64> {
    if round_trip_km == 0 {
        return None;
    }
    Some(settled_transport_minor.saturating_mul(100) / i64::from(round_trip_km))
}

/// Settled against predicted, or `None` when the prediction was zero.
fn relative_variance(predicted_minor: i64, settled_minor: i64) -> Option<i32> {
    if predicted_minor == 0 {
        return None;
    }
    let raw = i128::from(settled_minor.saturating_sub(predicted_minor)) * 10_000
        / i128::from(predicted_minor.abs());
    i32::try_from(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tour_economics::TransportBasis;

    const OFFERED_FEE: i64 = 200_000;

    fn predicted() -> ShowCost {
        ShowCost {
            transport_basis: TransportBasis::FlatRate,
            transport_minor: 100_000,
            vehicles: 2,
            round_trip_km: 500,
            nights_away: 1,
            rooms: 2,
            fuel_minor: 0,
            tolls_minor: 0,
            accommodation_minor: 40_000,
            per_diem_minor: 20_000,
            overhead_minor: 10_000,
            total_cost_minor: 170_000,
            net_margin_minor: 30_000,
            walk_away_fee_minor: 180_000,
        }
    }

    fn settled(transport: i64, other: i64, fee: i64) -> SettledShowCost {
        SettledShowCost {
            transport_minor: transport,
            accommodation_minor: 40_000,
            per_diem_minor: 20_000,
            overhead_minor: 10_000,
            other_minor: other,
            fee_received_minor: fee,
        }
    }

    #[test]
    fn a_model_inside_tolerance_is_left_alone() {
        let accuracy = assess_model_accuracy(
            predicted(),
            OFFERED_FEE,
            settled(105_000, 0, 200_000),
            SettlementPolicy::default(),
        );
        assert!(matches!(accuracy, ModelAccuracy::Calibrated { .. }));
        assert_eq!(accuracy.worst_line(), None);
    }

    #[test]
    fn the_worst_line_is_the_one_that_moved_the_most_money() {
        // Transport out by 60%, and a small line out by far more in percentage
        // terms. The operator should be sent to the road rate.
        let mut actual = settled(160_000, 0, 200_000);
        actual.per_diem_minor = 26_000;
        let accuracy = assess_model_accuracy(
            predicted(),
            OFFERED_FEE,
            actual,
            SettlementPolicy::default(),
        );
        assert_eq!(accuracy.worst_line(), Some(CostLine::Transport));
    }

    #[test]
    fn a_cost_the_model_has_no_line_for_is_named_as_such() {
        let accuracy = assess_model_accuracy(
            predicted(),
            OFFERED_FEE,
            settled(100_000, 80_000, 200_000),
            SettlementPolicy::default(),
        );
        assert_eq!(accuracy.worst_line(), Some(CostLine::Unmodelled));
    }

    #[test]
    fn a_fee_that_shrank_is_a_deal_finding_not_a_rate_finding() {
        let accuracy = assess_model_accuracy(
            predicted(),
            OFFERED_FEE,
            settled(100_000, 60_000, 120_000),
            SettlementPolicy::default(),
        );
        // The fee moved 80_000 and the unmodelled line 60_000, so the fee wins.
        assert_eq!(accuracy.worst_line(), Some(CostLine::Fee));
        assert!(CostLine::Fee.remedy().contains("deal terms"));
    }

    #[test]
    fn a_line_predicted_at_nothing_yields_no_percentage() {
        let variances = line_variances(predicted(), OFFERED_FEE, settled(100_000, 5_000, 200_000));
        let unmodelled = variances
            .into_iter()
            .find(|variance| variance.line == CostLine::Unmodelled)
            .expect("every line is reported");
        assert_eq!(unmodelled.variance_basis_points, None);
        assert_eq!(unmodelled.delta_minor(), 5_000);
    }

    #[test]
    fn a_small_absolute_miss_never_names_a_worst_line() {
        // Every line out by a few złoty. The percentages can be large and the
        // finding is still that nothing is wrong.
        let mut actual = settled(100_000, 2_000, 200_000);
        actual.overhead_minor = 12_000;
        let accuracy = assess_model_accuracy(
            predicted(),
            OFFERED_FEE,
            actual,
            SettlementPolicy::default(),
        );
        assert!(matches!(accuracy, ModelAccuracy::Calibrated { .. }));
    }

    #[test]
    fn the_settled_margin_is_what_arrived_minus_what_was_spent() {
        let actual = settled(160_000, 20_000, 150_000);
        assert_eq!(actual.total_cost_minor(), 250_000);
        assert_eq!(actual.net_margin_minor(), -100_000);
    }

    #[test]
    fn the_implied_road_rate_is_offered_only_when_the_distance_is_known() {
        assert_eq!(implied_transport_rate_minor_per_100km(100_000, 0), None);
        assert_eq!(
            implied_transport_rate_minor_per_100km(100_000, 500),
            Some(20_000)
        );
    }

    #[test]
    fn every_gap_names_itself_and_parses_back() {
        for gap in [
            SettlementGap::NoPrediction,
            SettlementGap::PredictionIncomplete,
            SettlementGap::NoSettlement,
        ] {
            assert_eq!(SettlementGap::parse(gap.as_str()), Some(gap));
        }
        for line in [
            CostLine::Transport,
            CostLine::Accommodation,
            CostLine::PerDiem,
            CostLine::Overhead,
            CostLine::Unmodelled,
            CostLine::Fee,
        ] {
            assert_eq!(CostLine::parse(line.as_str()), Some(line));
            assert!(!line.remedy().is_empty());
        }
    }
}
