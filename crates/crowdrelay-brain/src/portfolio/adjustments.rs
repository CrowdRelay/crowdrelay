//! Why a candidate's marginal value differs from its intrinsic value.
//!
//! The optimizer ranks on marginal value, which is the candidate's intrinsic
//! [`crate::decision_value::DecisionValue::total`] after the portfolio has had
//! its say: audience overlap, fatigue, and the uncalibrated-bridge discount.
//! Those arrived as three bare multipliers in one expression, so a candidate
//! that scored 7.2 on an intrinsic 10.0 could only be explained by
//! reverse-engineering the arithmetic — and only by someone who knew all three
//! factors existed.
//!
//! # Deltas, not factors
//!
//! Each adjustment is recorded in the same unit as the value it adjusts:
//! expected incremental Y30 fans. `intrinsic_y30` plus the three adjustments
//! equals `marginal_y30` exactly, by construction — each delta *is* the
//! difference the factor made at its point in the chain.
//!
//! # The honest caveat
//!
//! These are **sequential attributions of a product**, not independent
//! contributions. The factors are applied in a fixed order — overlap, then
//! fatigue, then bridge — and each delta is measured against the running value
//! at that point. Reorder them and the individual numbers change while the
//! total does not. That is a property of decomposing a product, not a defect,
//! and it is stated here rather than left for a reader to discover from a
//! number that does not add up the way they expected.
//!
//! # What this is not
//!
//! Not a framework, and not a place to add terms. Every adjustment here is a
//! portfolio *interaction* — a fact about this candidate alongside the others
//! already selected. A term that is a property of the candidate alone belongs
//! in `DecisionValue`, where the commensurate-units invariant governs it.

use serde::{Deserialize, Serialize};

/// The portfolio state and configuration that determine one candidate's
/// adjustments.
///
/// Taken as a struct rather than four positional floats: the previous
/// signature was `apply(intrinsic, overlap, fatigue, bridge)` with the factors
/// already computed by the caller, which meant the inputs that produced them
/// lived only in the caller's locals and the recorded deltas could not be
/// checked against anything. Passing the inputs and deriving the factors here
/// puts the arithmetic and the record in the same place, so they cannot
/// disagree.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdjustmentInputs {
    /// Selected candidates already sharing this candidate's audience.
    pub audience_count: u32,
    /// `PortfolioConfig::audience_overlap_penalty`.
    pub overlap_penalty: f64,
    /// `PortfolioConfig::fatigue_decay`.
    pub fatigue_decay: f64,
    /// The uncalibrated-bridge factor — `1.0` when it does not apply.
    pub bridge_factor: f64,
}

/// The portfolio's adjustments to one candidate's intrinsic value, in expected
/// incremental Y30 fans.
///
/// Built by [`Self::apply`] so the arithmetic and the record cannot disagree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MarginalAdjustments {
    /// `DecisionValue::total()` — what the candidate is worth on its own,
    /// before the portfolio.
    pub intrinsic_y30: f64,
    /// Diminishing returns from other selected candidates sharing this
    /// audience. Zero or negative.
    pub overlap_adjustment: f64,
    /// Decay from repeated dispatch to the same audience. Zero or negative.
    pub fatigue_adjustment: f64,
    /// Discount for a `Y14Bridged` estimate whose bridge is not yet
    /// calibrated — a Y14 number wearing a Y30 label. Zero or negative, and
    /// zero for every other regime.
    pub bridge_adjustment: f64,
    /// What the optimizer actually ranks on.
    pub marginal_y30: f64,

    // ── The inputs that produced the deltas above ──
    //
    // A delta says what a factor cost. These say why it cost that. Without
    // them `overlap_adjustment = -3.0` is a number a reader has to take on
    // faith: the count and the penalty that produced it are portfolio state
    // that does not survive the cycle, so the explanation would have to be
    // reconstructed from a configuration that has since been edited.
    //
    // Four numbers, chosen because together with `intrinsic_y30` they
    // determine all three deltas exactly. Nothing else about the portfolio is
    // stored here.
    /// Selected candidates already sharing this candidate's audience when it
    /// was scored. Drives both the overlap and fatigue factors.
    pub audience_count: u32,
    /// `PortfolioConfig::audience_overlap_penalty` in force at decision time.
    pub overlap_penalty: f64,
    /// `PortfolioConfig::fatigue_decay` in force at decision time.
    pub fatigue_decay: f64,
    /// The uncalibrated-bridge factor applied — `1.0` when the regime is not
    /// `Y14Bridged` or the bridge is calibrated.
    pub bridge_factor: f64,
}

impl MarginalAdjustments {
    /// Applies the portfolio factors in their fixed order and records what
    /// each one cost.
    ///
    /// The factors are multiplicative because diminishing returns are
    /// proportional — a second dispatch to an audience is worth a fraction of
    /// the first, not a fixed number of fans fewer. The *record* is additive
    /// because "this cost 1.2 fans" is the answerable question.
    #[must_use]
    pub fn apply(intrinsic_y30: f64, inputs: AdjustmentInputs) -> Self {
        let overlap = (1.0 - inputs.overlap_penalty * f64::from(inputs.audience_count)).max(0.0);
        let fatigue = inputs
            .fatigue_decay
            .powi(i32::try_from(inputs.audience_count).unwrap_or(i32::MAX));
        let after_overlap = intrinsic_y30 * overlap;
        let after_fatigue = after_overlap * fatigue;
        let after_bridge = after_fatigue * inputs.bridge_factor;
        Self {
            intrinsic_y30,
            overlap_adjustment: after_overlap - intrinsic_y30,
            fatigue_adjustment: after_fatigue - after_overlap,
            bridge_adjustment: after_bridge - after_fatigue,
            marginal_y30: after_bridge,
            audience_count: inputs.audience_count,
            overlap_penalty: inputs.overlap_penalty,
            fatigue_decay: inputs.fatigue_decay,
            bridge_factor: inputs.bridge_factor,
        }
    }

    /// True when the portfolio changed nothing — no overlap, no fatigue, no
    /// bridge discount. Useful for an operator surface that should not show a
    /// breakdown when there is nothing to break down.
    #[must_use]
    pub fn is_unadjusted(&self) -> bool {
        self.overlap_adjustment == 0.0
            && self.fatigue_adjustment == 0.0
            && self.bridge_adjustment == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(count: u32, penalty: f64, decay: f64, bridge: f64) -> AdjustmentInputs {
        AdjustmentInputs {
            audience_count: count,
            overlap_penalty: penalty,
            fatigue_decay: decay,
            bridge_factor: bridge,
        }
    }

    /// The record must reconstruct the number the optimizer ranked on.
    ///
    /// This is the whole contract. A breakdown whose parts do not reach the
    /// total is worse than no breakdown: it looks like an explanation and is
    /// not one, and a reader who trusts it will conclude the wrong thing about
    /// why a candidate lost.
    #[test]
    fn the_parts_reach_the_total() {
        for (intrinsic, count, penalty, decay, bridge) in [
            (10.0, 0, 0.3, 0.9, 1.0),
            (10.0, 1, 0.3, 0.9, 0.8),
            (10.0, 2, 0.3, 0.9, 1.0),
            (-3.0, 1, 0.3, 0.9, 0.8),
            (0.0, 1, 0.3, 0.9, 0.8),
            // Overlap clamps at zero rather than going negative.
            (10.0, 9, 0.3, 0.9, 1.0),
        ] {
            let adjustments =
                MarginalAdjustments::apply(intrinsic, inputs(count, penalty, decay, bridge));
            let summed = adjustments.intrinsic_y30
                + adjustments.overlap_adjustment
                + adjustments.fatigue_adjustment
                + adjustments.bridge_adjustment;
            assert!(
                (summed - adjustments.marginal_y30).abs() < 1e-9,
                "parts sum to {summed} but the marginal is {} for \
                 intrinsic={intrinsic} count={count}",
                adjustments.marginal_y30
            );
        }
    }

    /// The stored inputs reproduce the stored deltas.
    ///
    /// This is the replay contract, and it is stronger than the sum check: a
    /// reader months later has the record and nothing else — the audience
    /// count is portfolio state that vanished with the cycle, and the penalty
    /// and decay are configuration that has since been edited. Recomputing
    /// from the record must land on exactly what the record says.
    #[test]
    fn the_stored_inputs_reproduce_the_stored_deltas() {
        let original = MarginalAdjustments::apply(10.0, inputs(2, 0.3, 0.9, 0.8));

        // Everything a historical reader has.
        let replayed = MarginalAdjustments::apply(
            original.intrinsic_y30,
            AdjustmentInputs {
                audience_count: original.audience_count,
                overlap_penalty: original.overlap_penalty,
                fatigue_decay: original.fatigue_decay,
                bridge_factor: original.bridge_factor,
            },
        );

        assert_eq!(
            replayed, original,
            "a decision's adjustment record must be reproducible from itself"
        );
    }

    /// An untouched candidate says so, rather than reporting three zeros a
    /// reader has to add up.
    #[test]
    fn an_untouched_candidate_is_unadjusted() {
        let adjustments = MarginalAdjustments::apply(10.0, inputs(0, 0.3, 0.9, 1.0));
        assert!(adjustments.is_unadjusted());
        assert!((adjustments.marginal_y30 - 10.0).abs() < f64::EPSILON);

        let discounted = MarginalAdjustments::apply(10.0, inputs(0, 0.3, 0.9, 0.8));
        assert!(!discounted.is_unadjusted());
        assert!(
            (discounted.bridge_adjustment - (-2.0)).abs() < 1e-9,
            "a 0.8 bridge factor on 10.0 costs 2.0 fans, got {}",
            discounted.bridge_adjustment
        );
    }

    /// Each adjustment names the factor that produced it, and no other.
    ///
    /// The failure this prevents is a factor being wired into the wrong field
    /// — the arithmetic still totals correctly, and the explanation blames the
    /// wrong thing.
    #[test]
    fn each_adjustment_tracks_its_own_factor() {
        // Overlap only: one neighbour at a 0.5 penalty, no fatigue decay.
        let overlap_only = MarginalAdjustments::apply(10.0, inputs(1, 0.5, 1.0, 1.0));
        assert!((overlap_only.overlap_adjustment - (-5.0)).abs() < 1e-9);
        assert_eq!(overlap_only.fatigue_adjustment, 0.0);
        assert_eq!(overlap_only.bridge_adjustment, 0.0);

        // Fatigue only: one neighbour, no overlap penalty, 0.5 decay.
        let fatigue_only = MarginalAdjustments::apply(10.0, inputs(1, 0.0, 0.5, 1.0));
        assert_eq!(fatigue_only.overlap_adjustment, 0.0);
        assert!((fatigue_only.fatigue_adjustment - (-5.0)).abs() < 1e-9);
        assert_eq!(fatigue_only.bridge_adjustment, 0.0);
    }
}
