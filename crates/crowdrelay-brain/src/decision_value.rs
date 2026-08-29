//! DecisionValue — explicit provenance trail for every brain decision.
//!
//! Every decision the brain makes carries a full provenance trail — so
//! six months from now you can inspect a decision and know exactly why
//! it won. This is NOT a score. It is a transparent, inspectable record
//! of WHY a candidate has the value it has.
//!
//! # Critical Invariant
//!
//! `DecisionValue.total_value` is NOT a weighted soup. Every additive
//! term must have a defined conversion into expected incremental
//! fan-equivalent utility. Hard constraints (budget, reputation ceiling,
//! campaign slots) remain constraints in `PortfolioConfig`, not penalties
//! in `total_value`. No hand-tuned coefficients on quantities with
//! different units.

use serde::{Deserialize, Serialize};

use crate::causal_model::TreatmentAwareStats;
use crate::evidence::EvidenceQuality;
use crate::portfolio::DecisionMode;
use crate::resource_cost::ResourceCost;

/// The complete decision value for a single candidate — the canonical
/// object that the portfolio optimizer compares. Every term is in
/// expected incremental Y30 fans (or a directly comparable unit).
///
/// This is NOT a score. It is a transparent, inspectable record of
/// WHY a candidate has the value it has. The optimizer selects by
/// comparing `total_value` across candidates (including WAIT), but
/// every component is preserved for audit and learning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionValue {
    // ── Core prediction (from causal model) ──
    /// Expected incremental Y30 fans — the North Star. Absolute fans,
    /// NOT a ratio. This is the primary decision signal.
    pub expected_incremental_y30: f64,
    /// Uncertainty (posterior std) in the Y30 prediction.
    pub uncertainty: f64,
    /// P(τ > δ) — probability of meaningful effect.
    pub p_meaningful_effect: f64,

    // ── Evidence provenance ──
    /// The quality of evidence supporting this prediction.
    pub evidence_quality: EvidenceQuality,
    /// Sample size supporting the effect estimate.
    pub sample_size: u32,
    /// Whether Y30 is directly available or bridged from Y14.
    pub uses_y30: bool,
    /// Bridge confidence (0 = pure prior, 10+ = reliable).
    pub bridge_confidence: u32,

    // ── Resource economics ──
    /// The resource cost of this candidate.
    pub resource_cost: ResourceCost,

    // ── Value components (ALL in Y30 fan-equivalent utility) ──
    // CRITICAL INVARIANT: every additive term in total_value must have
    // a defined conversion into expected incremental fan-equivalent
    // utility. No hand-tuned coefficients on quantities with different
    // units. If a term cannot be expressed in fan-equivalents, it must
    // be a hard constraint (in PortfolioConfig), not a penalty here.
    //
    /// Pragmatic value: expected incremental Y30. This IS the North Star.
    pub pragmatic_value: f64,
    /// Epistemic value: expected fan-equivalent value of information
    /// gained by observing this action's outcome.
    pub epistemic_value: f64,
    /// Exploration value: expected fan-equivalent value of exploring
    /// a novel region of the action space.
    pub exploration_value: f64,
    /// Risk penalty: expected fan-equivalent loss from adverse outcomes.
    /// Negative.
    pub risk_penalty: f64,
    /// Opportunity cost: expected Y30 fans foregone by choosing this
    /// candidate instead of the next-best alternative. Negative.
    pub opportunity_cost: f64,

    // ── Decision mode ──
    /// Why the brain is dispatching this candidate.
    pub decision_mode: DecisionMode,
}

impl DecisionValue {
    /// Computes the total value from components.
    ///
    /// INVARIANT: every term must be in Y30 fan-equivalent utility.
    /// If a term cannot be expressed in fan-equivalents, it belongs
    /// in PortfolioConfig as a hard constraint, not here as a penalty.
    /// This prevents DecisionValue from becoming another arbitrary
    /// weighted soup like the old EFE.
    #[must_use]
    pub fn total(&self) -> f64 {
        self.pragmatic_value
            + self.epistemic_value
            + self.exploration_value
            + self.risk_penalty
            + self.opportunity_cost
    }

    /// Constructs a DecisionValue from treatment-aware stats and resource
    /// cost. The value components are derived from the causal model's
    /// predictions — no hand-tuned coefficients on quantities with
    /// different units.
    #[must_use]
    pub fn from_stats(
        stats: &TreatmentAwareStats,
        resource_cost: ResourceCost,
        decision_mode: DecisionMode,
    ) -> Self {
        let expected_y30 = if stats.use_treatment_effect {
            if stats.uses_y30 {
                stats.treatment_effect_y30
            } else {
                stats.treatment_effect
            }
        } else {
            stats.expected_fans
        };
        Self {
            expected_incremental_y30: expected_y30,
            uncertainty: if stats.uses_y30 {
                stats.treatment_std_y30
            } else {
                stats.treatment_std
            },
            p_meaningful_effect: stats.p_meaningful_effect,
            evidence_quality: EvidenceQuality::Observational,
            sample_size: if stats.uses_y30 {
                stats.treatment_confidence_y30
            } else {
                stats.treatment_confidence
            },
            uses_y30: stats.uses_y30,
            bridge_confidence: stats.bridge_confidence,
            resource_cost,
            pragmatic_value: expected_y30,
            epistemic_value: 0.0, // Phase 1: not yet computed in fan-value space
            exploration_value: 0.0, // Phase 1: not yet computed in fan-value space
            risk_penalty: 0.0,    // Phase 1: not yet computed in fan-value space
            opportunity_cost: 0.0, // Phase 1: computed by optimizer relative to next-best
            decision_mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_model::TreatmentAwareStats;
    use crate::resource_cost::ResourceCost;

    fn make_stats(expected: f64, std: f64, confidence: u32) -> TreatmentAwareStats {
        TreatmentAwareStats {
            expected_fans: expected,
            treatment_effect: expected,
            treatment_std: std,
            predict_std: std,
            confidence,
            treatment_confidence: confidence,
            use_treatment_effect: true,
            treatment_effect_y30: expected,
            treatment_std_y30: std,
            treatment_confidence_y30: confidence,
            uses_y30: true,
            p_meaningful_effect: 0.8,
            bridge_confidence: 5,
            bridge_is_reliable: false,
        }
    }

    #[test]
    fn total_sums_all_components() {
        let dv = DecisionValue {
            expected_incremental_y30: 5.0,
            uncertainty: 2.0,
            p_meaningful_effect: 0.8,
            evidence_quality: EvidenceQuality::Observational,
            sample_size: 10,
            uses_y30: true,
            bridge_confidence: 5,
            resource_cost: ResourceCost::configured(1.0),
            pragmatic_value: 5.0,
            epistemic_value: 0.5,
            exploration_value: 0.3,
            risk_penalty: -0.2,
            opportunity_cost: -1.0,
            decision_mode: DecisionMode::Exploit,
        };
        assert!((dv.total() - 4.6).abs() < 0.001);
    }

    #[test]
    fn from_stats_derives_expected_y30() {
        let stats = make_stats(5.0, 2.0, 10);
        let dv =
            DecisionValue::from_stats(&stats, ResourceCost::configured(1.0), DecisionMode::Exploit);
        assert!((dv.expected_incremental_y30 - 5.0).abs() < 0.001);
        assert!((dv.pragmatic_value - 5.0).abs() < 0.001);
        assert!(dv.uses_y30);
        assert_eq!(dv.sample_size, 10);
    }

    #[test]
    fn from_stats_uses_y14_when_y30_not_available() {
        let mut stats = make_stats(3.0, 4.0, 5);
        stats.uses_y30 = false;
        stats.use_treatment_effect = true;
        stats.treatment_effect_y30 = 0.0; // Y30 not available
        let dv =
            DecisionValue::from_stats(&stats, ResourceCost::configured(1.0), DecisionMode::Learn);
        // Should use treatment_effect (Y14-bridged) not treatment_effect_y30
        assert!((dv.expected_incremental_y30 - 3.0).abs() < 0.001);
        assert!(!dv.uses_y30);
    }

    #[test]
    fn round_trips_through_serde() {
        let dv = DecisionValue {
            expected_incremental_y30: 5.0,
            uncertainty: 2.0,
            p_meaningful_effect: 0.8,
            evidence_quality: EvidenceQuality::RandomizedHoldout,
            sample_size: 10,
            uses_y30: true,
            bridge_confidence: 5,
            resource_cost: ResourceCost::configured(2.0),
            pragmatic_value: 5.0,
            epistemic_value: 0.5,
            exploration_value: 0.3,
            risk_penalty: -0.2,
            opportunity_cost: -1.0,
            decision_mode: DecisionMode::Exploit,
        };
        let json = serde_json::to_string(&dv).unwrap();
        let back: DecisionValue = serde_json::from_str(&json).unwrap();
        assert!((back.expected_incremental_y30 - 5.0).abs() < 0.001);
        assert_eq!(back.evidence_quality, EvidenceQuality::RandomizedHoldout);
        assert_eq!(back.decision_mode, DecisionMode::Exploit);
    }
}
