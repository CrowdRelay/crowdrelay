//! DecisionValue — explicit provenance trail for every brain decision.
//!
//! Every decision the brain makes carries a full provenance trail — so
//! six months from now you can inspect a decision and know exactly why
//! it won. This is NOT a score. It is a transparent, inspectable record
//! of WHY a candidate has the value it has.
//!
//! # Critical Invariant
//!
//! `DecisionValue.total()` is NOT a weighted soup. Every additive
//! term must have a defined conversion into expected incremental
//! fan-equivalent utility. Hard constraints (budget, reputation ceiling,
//! campaign slots) remain constraints in `PortfolioConfig`, not penalties
//! in `total()`. No hand-tuned coefficients on quantities with
//! different units.
//!
//! # EFE ≠ DecisionValue
//!
//! EFE (Expected Free Energy) decides what is worth *learning about*
//! (candidate generation, exploration allocation). DecisionValue decides
//! what is worth *doing* (portfolio ranking). The portfolio optimizer
//! must never combine EFE with `DecisionValue.total()`. EFE signals
//! are carried in `PortfolioCandidate::generation_signal` as non-economic
//! provenance — the optimizer ignores them for ranking.
//!
//! # UNCERTAINTY ≠ RISK
//!
//! Uncertainty = "I don't know how large the effect is" (posterior std).
//! Risk = "The downside if this action is wrong or harmful."
//! These are separate concepts. Risk is NEVER derived from uncertainty.
//! Phase 1: risk is `None` (NotModeled). Future: risk may become hard
//! constraints (risk > ceiling → candidate ineligible) or explicit
//! downside utility — but never a function of prediction uncertainty.
//!
//! # Architecture
//!
//! `DecisionValue` = **intrinsic** value of a candidate before portfolio
//! interactions. The `PortfolioOptimizer` computes **marginal** value from
//! it after applying audience overlap, fatigue, and resource constraints.
//! This separation prevents contaminating the intrinsic value with
//! dynamic portfolio state.

use serde::{Deserialize, Serialize};

use crate::causal_model::TreatmentAwareStats;
use crate::evidence::EvidenceQuality;
use crate::portfolio::DecisionMode;
use crate::resource_cost::ResourceCost;

/// How `expected_incremental_y30` was estimated. The regime determines
/// the trust level and whether bridge uncertainty should be applied.
///
/// The optimizer does NOT refuse to compare candidates across regimes —
/// that would paralyze a young learning system where different templates
/// have different evidence maturity. Instead, the regime's reliability
/// is accounted for via `uncertainty` and `bridge_is_reliable`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimationRegime {
    /// Y30 treatment-effect posterior — directly observed durable fans.
    /// Highest trust. No bridge uncertainty.
    Y30Direct,
    /// Y14 treatment effect + Y14→Y30 bridge model. The bridge inflates
    /// variance when uncalibrated. Medium trust.
    Y14Bridged,
    /// Outcome model only — no treatment-effect evidence. Observational.
    /// Lowest causal confidence.
    OutcomeModel,
}

impl EstimationRegime {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Y30Direct => "y30_direct",
            Self::Y14Bridged => "y14_bridged",
            Self::OutcomeModel => "outcome_model",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "y30_direct" => Some(Self::Y30Direct),
            "y14_bridged" => Some(Self::Y14Bridged),
            "outcome_model" => Some(Self::OutcomeModel),
            _ => None,
        }
    }
}

/// The complete decision value for a single candidate — the canonical
/// object that the portfolio optimizer compares. Every additive term is
/// in expected incremental Y30 fans (or a directly comparable unit).
///
/// This is NOT a score. It is a transparent, inspectable record of
/// WHY a candidate has the value it has. The optimizer selects by
/// comparing `total()` across candidates (including WAIT), but
/// every component is preserved for audit and learning.
///
/// `DecisionValue` represents the **intrinsic** value of the candidate
/// before portfolio interactions (overlap, fatigue, budget). The
/// optimizer computes **marginal** value from it.
///
/// # Provenance
///
/// Every DecisionValue carries full provenance: estimation regime,
/// evidence quality, sample size, bridge confidence, contamination,
/// and calibration bias. This lets the brain answer "why did you choose
/// A?" with inspectable evidence, not a single confidence number.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionValue {
    // ── Core prediction (from causal model) ──
    /// Expected incremental Y30 fans — the North Star. Absolute fans,
    /// NOT a ratio. This is the primary decision signal.
    pub expected_incremental_y30: f64,
    /// Uncertainty (posterior std) in the Y30 prediction.
    /// This is "I don't know how large the effect is" — NOT risk.
    pub uncertainty: f64,
    /// P(τ > δ) — probability of meaningful effect.
    pub p_meaningful_effect: f64,
    /// How `expected_incremental_y30` was estimated. Determines trust
    /// level and whether bridge uncertainty applies.
    pub estimation_regime: EstimationRegime,

    // ── Evidence provenance ──
    /// The quality of evidence supporting this prediction.
    pub evidence_quality: EvidenceQuality,
    /// Sample size supporting the effect estimate.
    pub sample_size: u32,
    /// Whether Y30 is directly available or bridged from Y14.
    pub uses_y30: bool,
    /// Bridge confidence (0 = pure prior, 10+ = reliable).
    pub bridge_confidence: u32,
    /// Whether the bridge is reliable enough to trust Y14-bridged Y30
    /// estimates. True when `bridge_confidence >= MIN_BRIDGE_CONFIDENCE`.
    /// When false, the optimizer should apply a confidence penalty
    /// to Y14Bridged candidates.
    pub bridge_is_reliable: bool,
    /// Estimated contamination from concurrent actions (0.0–1.0).
    /// 0.0 = clean, 1.0 = fully contaminated. Derived from the
    /// experiment assignment's contamination estimate. High
    /// contamination downgrades evidence quality.
    #[serde(default)]
    pub contamination: f64,
    /// Calibration bias for this template — the running mean of
    /// (predicted - observed) Y30. Positive = overestimating,
    /// negative = underestimating. 0.0 when no calibration data.
    /// Applied to make predictions more honest.
    #[serde(default)]
    pub calibration_bias: f64,

    // ── Resource economics ──
    /// The resource cost of this candidate.
    pub resource_cost: ResourceCost,

    // ── Value components (ALL in Y30 fan-equivalent utility) ──
    // CRITICAL INVARIANT: every additive term in total() must have
    // a defined conversion into expected incremental fan-equivalent
    // utility. No hand-tuned coefficients on quantities with different
    // units. If a term cannot be expressed in fan-equivalents, it must
    // be a hard constraint (in PortfolioConfig), not a penalty here.
    //
    // EFE-derived terms (epistemic_value, exploration_value) are NOT
    // here — they belong to EFE (candidate generation), not to the
    // economic value of a candidate. The optimizer must never add
    // EFE signals to total().
    //
    /// Pragmatic value: expected incremental Y30. This IS the North Star.
    pub pragmatic_value: f64,
    /// Risk penalty: expected fan-equivalent loss from adverse outcomes.
    /// Negative when modeled.
    ///
    /// `None` = risk is NotModeled (Phase 1). This does NOT mean "risk
    /// is zero" — it means "we have not yet modeled risk." Future: risk
    /// may become hard constraints (risk > ceiling → ineligible) or
    /// explicit downside utility. NEVER derived from uncertainty.
    #[serde(default)]
    pub risk_penalty: Option<f64>,
    /// Opportunity cost: expected Y30 fans foregone by choosing this
    /// candidate instead of the next-best alternative. Negative.
    /// Computed by the optimizer relative to the portfolio, not here.
    pub opportunity_cost: f64,

    // ── Decision mode ──
    /// Why the brain is dispatching this candidate.
    pub decision_mode: DecisionMode,
}

impl DecisionValue {
    /// Computes the total intrinsic value from components.
    ///
    /// INVARIANT: every term must be in Y30 fan-equivalent utility.
    /// If a term cannot be expressed in fan-equivalents, it belongs
    /// in PortfolioConfig as a hard constraint, not here as a penalty.
    /// This prevents DecisionValue from becoming another arbitrary
    /// weighted soup like the old EFE.
    ///
    /// `risk_penalty` is `None` (NotModeled) in Phase 1 — treated as
    /// 0.0. This is semantically "risk not yet modeled", NOT "risk
    /// is proven zero."
    #[must_use]
    pub fn total(&self) -> f64 {
        self.pragmatic_value + self.risk_penalty.unwrap_or(0.0) + self.opportunity_cost
    }

    /// Constructs a DecisionValue from treatment-aware stats and resource
    /// cost. The value components are derived from the causal model's
    /// predictions — no hand-tuned coefficients on quantities with
    /// different units.
    ///
    /// The `estimation_regime` is derived from the stats:
    /// - `uses_y30 && use_treatment_effect` → `Y30Direct`
    /// - `!uses_y30 && use_treatment_effect` → `Y14Bridged`
    /// - `!use_treatment_effect` → `OutcomeModel`
    ///
    /// The `evidence_quality` is taken from the stats (which carry the
    /// strongest evidence quality available for this template/context).
    ///
    /// `risk_penalty` is `None` (NotModeled) — Phase 1 does not model
    /// risk. This is NOT "risk = 0" — it is "risk not yet modeled."
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
        let estimation_regime = if stats.use_treatment_effect {
            if stats.uses_y30 {
                EstimationRegime::Y30Direct
            } else {
                EstimationRegime::Y14Bridged
            }
        } else {
            EstimationRegime::OutcomeModel
        };
        Self {
            expected_incremental_y30: expected_y30,
            uncertainty: if !stats.use_treatment_effect {
                // OutcomeModel regime: use the outcome model's prediction std.
                stats.predict_std
            } else if stats.uses_y30 {
                stats.treatment_std_y30
            } else {
                stats.treatment_std
            },
            p_meaningful_effect: stats.p_meaningful_effect,
            estimation_regime,
            evidence_quality: stats.evidence_quality,
            sample_size: if !stats.use_treatment_effect {
                // OutcomeModel regime: use the outcome model's confidence.
                stats.confidence
            } else if stats.uses_y30 {
                stats.treatment_confidence_y30
            } else {
                stats.treatment_confidence
            },
            uses_y30: stats.uses_y30,
            bridge_confidence: stats.bridge_confidence,
            bridge_is_reliable: stats.bridge_is_reliable,
            contamination: 0.0,    // Wired in Step 7 from experiment state
            calibration_bias: 0.0, // Wired in Step 7 from calibration tracker
            resource_cost,
            pragmatic_value: expected_y30,
            risk_penalty: None,    // Phase 1: NotModeled. NOT "risk = 0".
            opportunity_cost: 0.0, // Computed by optimizer relative to next-best
            decision_mode,
        }
    }

    /// Sets the contamination estimate on this DecisionValue. Called by
    /// the application layer after loading contamination from the
    /// experiment assignment state. Returns `self` for chaining.
    #[must_use]
    pub fn with_contamination(mut self, contamination: f64) -> Self {
        self.contamination = contamination.clamp(0.0, 1.0);
        self
    }

    /// Sets the calibration bias on this DecisionValue. Called by the
    /// application layer after loading calibration from the calibration
    /// tracker. Returns `self` for chaining.
    #[must_use]
    pub fn with_calibration_bias(mut self, calibration_bias: f64) -> Self {
        self.calibration_bias = calibration_bias;
        self
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
            evidence_quality: EvidenceQuality::Observational,
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
            estimation_regime: EstimationRegime::Y30Direct,
            evidence_quality: EvidenceQuality::Observational,
            sample_size: 10,
            uses_y30: true,
            bridge_confidence: 5,
            bridge_is_reliable: false,
            contamination: 0.0,
            calibration_bias: 0.0,
            resource_cost: ResourceCost::configured(1.0),
            pragmatic_value: 5.0,
            risk_penalty: Some(-0.2),
            opportunity_cost: -1.0,
            decision_mode: DecisionMode::Exploit,
        };
        // total = 5.0 + (-0.2) + (-1.0) = 3.8
        assert!((dv.total() - 3.8).abs() < 0.001);
    }

    #[test]
    fn total_with_unmodeled_risk_treats_as_zero() {
        let dv = DecisionValue {
            expected_incremental_y30: 5.0,
            uncertainty: 2.0,
            p_meaningful_effect: 0.8,
            estimation_regime: EstimationRegime::Y30Direct,
            evidence_quality: EvidenceQuality::Observational,
            sample_size: 10,
            uses_y30: true,
            bridge_confidence: 5,
            bridge_is_reliable: false,
            contamination: 0.0,
            calibration_bias: 0.0,
            resource_cost: ResourceCost::configured(1.0),
            pragmatic_value: 5.0,
            risk_penalty: None, // NotModeled
            opportunity_cost: -1.0,
            decision_mode: DecisionMode::Exploit,
        };
        // total = 5.0 + 0.0 + (-1.0) = 4.0
        assert!((dv.total() - 4.0).abs() < 0.001);
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
        assert_eq!(dv.estimation_regime, EstimationRegime::Y30Direct);
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
        assert_eq!(dv.estimation_regime, EstimationRegime::Y14Bridged);
    }

    #[test]
    fn from_stats_outcome_model_when_no_treatment_effect() {
        let mut stats = make_stats(3.0, 4.0, 5);
        stats.use_treatment_effect = false;
        let dv =
            DecisionValue::from_stats(&stats, ResourceCost::configured(1.0), DecisionMode::Explore);
        assert_eq!(dv.estimation_regime, EstimationRegime::OutcomeModel);
        // Should use expected_fans (outcome model)
        assert!((dv.expected_incremental_y30 - 3.0).abs() < 0.001);
    }

    #[test]
    fn from_stats_propagates_evidence_quality() {
        let mut stats = make_stats(5.0, 2.0, 10);
        stats.evidence_quality = EvidenceQuality::RandomizedHoldout;
        let dv =
            DecisionValue::from_stats(&stats, ResourceCost::configured(1.0), DecisionMode::Exploit);
        assert_eq!(dv.evidence_quality, EvidenceQuality::RandomizedHoldout);
    }

    #[test]
    fn from_stats_propagates_bridge_reliability() {
        let mut stats = make_stats(5.0, 2.0, 10);
        stats.bridge_is_reliable = true;
        stats.bridge_confidence = 15;
        let dv =
            DecisionValue::from_stats(&stats, ResourceCost::configured(1.0), DecisionMode::Exploit);
        assert!(dv.bridge_is_reliable);
        assert_eq!(dv.bridge_confidence, 15);
    }

    #[test]
    fn from_stats_risk_is_not_modeled() {
        let stats = make_stats(5.0, 2.0, 10);
        let dv =
            DecisionValue::from_stats(&stats, ResourceCost::configured(1.0), DecisionMode::Exploit);
        // Phase 1: risk is NotModeled (None), NOT zero.
        assert!(dv.risk_penalty.is_none());
    }

    #[test]
    fn estimation_regime_round_trips() {
        for regime in [
            EstimationRegime::Y30Direct,
            EstimationRegime::Y14Bridged,
            EstimationRegime::OutcomeModel,
        ] {
            assert_eq!(EstimationRegime::parse(regime.as_str()), Some(regime));
        }
    }

    #[test]
    fn round_trips_through_serde() {
        let dv = DecisionValue {
            expected_incremental_y30: 5.0,
            uncertainty: 2.0,
            p_meaningful_effect: 0.8,
            estimation_regime: EstimationRegime::Y30Direct,
            evidence_quality: EvidenceQuality::RandomizedHoldout,
            sample_size: 10,
            uses_y30: true,
            bridge_confidence: 5,
            bridge_is_reliable: false,
            contamination: 0.1,
            calibration_bias: -0.3,
            resource_cost: ResourceCost::configured(2.0),
            pragmatic_value: 5.0,
            risk_penalty: Some(-0.5),
            opportunity_cost: -1.0,
            decision_mode: DecisionMode::Exploit,
        };
        let json = serde_json::to_string(&dv).unwrap();
        let back: DecisionValue = serde_json::from_str(&json).unwrap();
        assert!((back.expected_incremental_y30 - 5.0).abs() < 0.001);
        assert_eq!(back.evidence_quality, EvidenceQuality::RandomizedHoldout);
        assert_eq!(back.estimation_regime, EstimationRegime::Y30Direct);
        assert_eq!(back.decision_mode, DecisionMode::Exploit);
        assert!(!back.bridge_is_reliable);
        assert!((back.contamination - 0.1).abs() < 0.001);
        assert!((back.calibration_bias - (-0.3)).abs() < 0.001);
        assert_eq!(back.risk_penalty, Some(-0.5));
    }

    #[test]
    fn serde_backwards_compatible_with_old_risk_as_f64() {
        // Old brain state checkpoints may have risk_penalty as f64 (not Option).
        // A bare number deserializes as Some(value) via serde's Option handling.
        let old_json = r#"{
            "expected_incremental_y30": 5.0,
            "uncertainty": 2.0,
            "p_meaningful_effect": 0.8,
            "estimation_regime": "y30_direct",
            "evidence_quality": "observational",
            "sample_size": 10,
            "uses_y30": true,
            "bridge_confidence": 5,
            "bridge_is_reliable": false,
            "resource_cost": {"units": 1.0, "source": "configured"},
            "pragmatic_value": 5.0,
            "risk_penalty": -0.2,
            "opportunity_cost": -1.0,
            "decision_mode": "exploit"
        }"#;
        let back: DecisionValue = serde_json::from_str(old_json).unwrap();
        assert!((back.expected_incremental_y30 - 5.0).abs() < 0.001);
        assert_eq!(back.risk_penalty, Some(-0.2));
    }

    #[test]
    fn serde_handles_missing_new_fields() {
        // Brain state checkpoints from before this sprint won't have
        // contamination or calibration_bias. #[serde(default)] handles this.
        let old_json = r#"{
            "expected_incremental_y30": 5.0,
            "uncertainty": 2.0,
            "p_meaningful_effect": 0.8,
            "estimation_regime": "y30_direct",
            "evidence_quality": "observational",
            "sample_size": 10,
            "uses_y30": true,
            "bridge_confidence": 5,
            "bridge_is_reliable": false,
            "resource_cost": {"units": 1.0, "source": "configured"},
            "pragmatic_value": 5.0,
            "opportunity_cost": -1.0,
            "decision_mode": "exploit"
        }"#;
        let back: DecisionValue = serde_json::from_str(old_json).unwrap();
        assert!((back.contamination - 0.0).abs() < 0.001);
        assert!((back.calibration_bias - 0.0).abs() < 0.001);
        assert!(back.risk_penalty.is_none());
    }
}
