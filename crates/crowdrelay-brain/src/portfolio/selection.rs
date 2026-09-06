//! What a selection produced, and why each candidate was left out.
//!
//! The outcome side of the optimizer, split from the algorithm so `portfolio.rs`
//! stays inside the source-size ratchet. Nothing here decides anything; it is
//! the record a reader consults to find out what the optimizer did.

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use super::{MarginalAdjustments, PortfolioCandidate};

/// The result of portfolio optimization.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PortfolioSelection {
    /// The selected candidates, in dispatch priority order.
    pub selected: Vec<PortfolioCandidate>,
    /// The candidates that were not selected (and why).
    pub rejected: Vec<PortfolioRejection>,
    /// The sum of the selected candidates' **marginal** values.
    ///
    /// Not the sum of their expected fans, which is what the name says and
    /// what a reader comparing it against `DecisionValue::pragmatic_value`
    /// would assume. Each term has already been through the portfolio
    /// interactions — audience overlap, fatigue, and the uncalibrated-bridge
    /// discount — so it is what the portfolio expects to gain *given the rest
    /// of the portfolio*, and it is at or below the sum of the intrinsic
    /// values by construction.
    ///
    /// The field name is the API contract and is left alone.
    pub total_expected_fans: f64,
    /// Whether "DO NOTHING" was selected (all candidates had negative value).
    pub do_nothing: bool,
    /// When `do_nothing` is true, the economic rationale for WAIT.
    /// Explains why waiting produces more expected Y30 fan value than
    /// dispatching any available candidate.
    pub wait_reason: Option<String>,
    /// Why each selected candidate's marginal differs from its intrinsic
    /// value, keyed by opportunity key.
    ///
    /// The optimizer ranks on the marginal, and the marginal is the intrinsic
    /// value after overlap, fatigue and the uncalibrated-bridge discount. Those
    /// were three bare multipliers inside one expression: a candidate that
    /// scored 7.2 on an intrinsic 10.0 could only be explained by someone who
    /// already knew all three existed. This is that explanation, recorded at
    /// the moment it was true.
    ///
    /// Selected candidates only. A rejection carries its [`RejectionReason`],
    /// which answers a different question — a candidate rejected for
    /// `BudgetExhausted` never had a marginal computed against the final
    /// portfolio, so a breakdown for it would be a number from a different
    /// world.
    #[serde(default)]
    pub marginal_adjustments: BTreeMap<String, MarginalAdjustments>,
}

/// A rejected candidate and the reason for rejection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioRejection {
    pub opportunity_key: String,
    pub reason: RejectionReason,
}

/// Why a candidate was not selected for the portfolio.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// The marginal value was negative (not worth dispatching).
    NegativeMarginalValue,
    /// The marginal value was positive but below the minimum threshold.
    BelowThreshold,
    /// The dispatch count budget (max_dispatches) was exhausted.
    MaxDispatchesReached,
    /// The cost budget was exhausted.
    BudgetExhausted,
    /// The candidate was superseded by a better candidate for the same audience.
    Superseded,
}
