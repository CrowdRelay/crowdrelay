//! What waiting is worth.
//!
//! Its own module because it is the one candidate in the portfolio that is not
//! an action, and the question it answers — what does the brain gain by not
//! acting — has different inputs from everything else in the pool. Splitting it
//! also keeps `portfolio.rs` inside the source-size ratchet.

use serde::{Deserialize, Serialize};

/// The value of WAIT (doing nothing) expressed in expected incremental
/// Y30 fans. Every term is in the **same fan-value utility space** —
/// no mixed-unit scalar soup.
///
/// WAIT does NOT become more valuable merely because many expensive
/// candidates exist. `avoided_cost` is NOT a term — resource cost is
/// already captured in the action's value. WAIT's value comes from
/// information, fatigue recovery, and option value, minus the
/// opportunity cost of not acting.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WaitCandidateValue {
    /// Value of information from pending measurements, expressed
    /// in expected incremental Y30 fans. Computed as:
    ///   VOI = count_pending * avg_treatment_std * DECISION_SENSITIVITY
    /// where DECISION_SENSITIVITY converts uncertainty to expected
    /// fan value — calibrated empirically later.
    pub value_of_information: f64,
    /// Fatigue recovery value in expected Y30 fans.
    ///
    /// **Not computed. Always 0.0.** This comment used to give a formula —
    /// `sum(audience_fatigue * fatigue_recovery_per_cycle * expected_fans)` —
    /// as though it described code. Nothing computes it; every call site of
    /// [`Self::compute`] passes `0.0`, and the parameter exists as the seam
    /// where it would arrive.
    ///
    /// Say which way that biases the comparison, because it is not neutral.
    /// This term and [`Self::option_value`] are the two that would make
    /// waiting *more* valuable, and both are zero, so WAIT competes on
    /// value-of-information against a full opportunity cost. The brain is
    /// therefore biased toward acting by however much a recovered audience is
    /// worth. Guessing a coefficient to close that gap would be worse: an
    /// invented number in fan-equivalent units is exactly the weighted soup
    /// `DecisionValue` refuses, and it would be indistinguishable from a
    /// measured one.
    pub fatigue_recovery_value: f64,
    /// Opportunity cost of NOT acting now. This is NEGATIVE.
    ///   -best_candidate_expected_y30
    /// Waiting costs the fan value we could have gained now.
    pub opportunity_cost: f64,
    /// Preserved option value — placeholder, 0.0 for now.
    /// Future: V(wait) = E[best_future_action] - immediate_action_value
    pub option_value: f64,
}

impl WaitCandidateValue {
    /// The total WAIT utility — sum of all components.
    /// Every term is in expected incremental Y30 fans.
    #[must_use]
    pub fn total(&self) -> f64 {
        self.value_of_information
            + self.fatigue_recovery_value
            + self.option_value
            + self.opportunity_cost
    }

    /// What waiting is worth for reasons that acting cannot supply.
    ///
    /// The distinction `min_dispatches` needs. WAIT wins for two different kinds
    /// of reason, and only one of them is a deadlock:
    ///
    /// - **Waiting for information.** [`Self::value_of_information`] is high
    ///   because measurements are pending, and a pending measurement resolves
    ///   only after the brain acts and the window elapses. Honouring this WAIT
    ///   means never acting, never resolving, and never learning.
    ///   `min_dispatches` exists to break exactly this, and acting is the only
    ///   escape.
    ///
    /// - **Waiting for the fanbase.** [`Self::fatigue_recovery_value`] is high
    ///   because the audience is tired. Acting does not resolve that; acting
    ///   makes it worse. Overriding this WAIT is spam, which the North Star
    ///   rules out, and it burns the fans the whole system exists to grow.
    ///
    /// This sum excludes VOI, so it answers: would WAIT still win if the brain
    /// already knew everything the pending measurements could teach it? If yes,
    /// `min_dispatches` must not override, because no amount of acting changes
    /// that answer.
    ///
    /// **Zero today, on purpose.** `fatigue_recovery_value` and `option_value`
    /// are both unconditionally `0.0` — see the fields — so this is currently
    /// just the negative opportunity cost and is never positive. Every WAIT that
    /// can win today is a VOI deadlock, which is why overriding unconditionally
    /// has been indistinguishable from overriding correctly. The distinction is
    /// drawn now, while both readings agree, so that filling either seam changes
    /// one number rather than silently turning the override into a spam switch.
    #[must_use]
    pub fn total_excluding_information(&self) -> f64 {
        self.fatigue_recovery_value + self.option_value + self.opportunity_cost
    }

    /// The decision sensitivity constant — converts treatment uncertainty
    /// to expected fan value. Conservative: 0.1 means VOI is small relative
    /// to typical fan values (2-5). Monitor in production — if WAIT never
    /// wins, increase; if it always wins, decrease.
    const DECISION_SENSITIVITY: f64 = 0.1;

    /// Cap on the number of pending measurements that contribute to VOI.
    ///
    /// The marginal information value of each additional pending measurement
    /// diminishes — the 34th measurement teaches less than the 1st. Without
    /// a cap, a large measurement backlog inflates VOI and WAIT wins every
    /// cycle, creating a cold-start deadlock: the brain won't act because
    /// it's waiting for measurements, but measurements only resolve after
    /// the brain acts and the measurement window elapses.
    ///
    /// The cap keeps VOI proportional to the information the brain can
    /// actually absorb in one cycle, not the total backlog size.
    const VOI_PENDING_MEASUREMENT_CAP: u32 = 10;

    /// Computes the WAIT candidate value from the current state.
    ///
    /// - `best_candidate_expected_y30`: the highest expected Y30 among
    ///   available action candidates (0.0 if no candidates).
    /// - `count_pending_measurements`: number of measurements whose
    ///   outcomes haven't been observed yet.
    /// - `avg_treatment_std`: average treatment-effect std across
    ///   pending candidates.
    /// - `fatigue_recovery_value`: the seam for fatigue recovery in fan-value
    ///   space. Every caller passes `0.0` — nothing computes it. See the field.
    #[must_use]
    pub fn compute(
        best_candidate_expected_y30: f64,
        count_pending_measurements: u32,
        avg_treatment_std: f64,
        fatigue_recovery_value: f64,
    ) -> Self {
        let capped_pending = count_pending_measurements.min(Self::VOI_PENDING_MEASUREMENT_CAP);
        Self {
            value_of_information: f64::from(capped_pending)
                * avg_treatment_std
                * Self::DECISION_SENSITIVITY,
            fatigue_recovery_value,
            opportunity_cost: -best_candidate_expected_y30,
            option_value: 0.0,
        }
    }
}
