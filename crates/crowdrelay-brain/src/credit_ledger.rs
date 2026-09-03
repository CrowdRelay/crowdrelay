//! Credit Ledger — causal credit allocation with anti-self-reinforcement.
//!
//! When a fan outcome is observed, the credit ledger allocates credit
//! among the competing actions that could have produced it. The allocation
//! is driven **primarily** by observable facts (exposure, temporal
//! proximity, audience match), with the model posterior as a **bounded**
//! prior — capped at ~20% influence. This prevents the feedback amplifier:
//! `prior → more credit → more evidence → stronger prior → more credit`.
//!
//! # Critical Invariant
//!
//! `RAW FACT ≠ ATTRIBUTION ≠ CAUSAL EFFECT ≠ PREDICTION ≠ DECISION VALUE`
//!
//! The credit ledger stores **attributed** credit — a separate layer from
//! the raw observation (which is immutable in the evidence table). The
//! learner consumes credited effects from the credit ledger, while raw
//! evidence remains available for replay, recalculation, and future
//! attribution-method upgrades.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::evidence::EvidenceQuality;

/// The maximum influence the model posterior can have on credit allocation.
/// Capped at 20% to prevent self-reinforcement: the prior can nudge credit
/// allocation, but never dominate it.
const BOUNDED_PRIOR_WEIGHT: f64 = 0.2;

/// A fan outcome observation — the raw, immutable fact that N incremental
/// fans were observed in a measurement window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FanOutcome {
    pub workspace_id: uuid::Uuid,
    /// The raw observed incremental fan count. This is a FACT, not an
    /// attribution — it stays immutable in the evidence table.
    pub observed_incremental_fans: f64,
    /// Durable fans at 30 days, if the Y30 window has elapsed.
    pub durable_fans_30d: Option<f64>,
    pub measurement_window_start: OffsetDateTime,
    pub measurement_window_end: OffsetDateTime,
}

/// An action that was exposed to the audience during the measurement
/// window — a candidate for credit allocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionExposure {
    pub action_id: uuid::Uuid,
    pub template_id: String,
    pub audience_key: String,
    /// Whether the action actually delivered exposure (post was made,
    /// email was sent, etc.). Zero credit if false.
    pub exposure_delivered: bool,
    /// 0.0–1.0, 1.0 = closest to the outcome window.
    pub temporal_proximity: f64,
    /// 0.0–1.0, 1.0 = perfect audience/target match.
    pub audience_match: f64,
    /// 0.0–1.0, confidence in the attribution itself.
    pub attribution_confidence: f64,
    /// Model posterior mean — used only as a BOUNDED prior,
    /// not the dominant allocator. Capped at a small influence
    /// to prevent self-reinforcement.
    pub treatment_effect_prior: f64,
    /// Evidence quality of this action's causal estimate.
    pub evidence_quality: EvidenceQuality,
}

/// The result of credit allocation — credits per action plus an
/// unattributed residual. The residual is always preserved: "we don't
/// know" is a valid outcome, and no forced 100% attribution is applied.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttributionResult {
    pub credits: Vec<CreditEntry>,
    /// Always preserved — no forced 100% attribution.
    /// `unattributed = observed_incremental - sum(credited_y14)`
    pub unattributed: f64,
    pub method: AttributionMethod,
}

/// A single credit entry — the attributed share of the fan outcome
/// for one action.
///
/// ATTRIBUTION CREDIT ≠ CAUSAL EFFECT EVIDENCE. These are attribution
/// artifacts, not causal evidence. The learner must consume them with
/// `EvidenceQuality::ModeledAttribution` weighting. Only randomized/
/// quasi-experimental evidence produces causal claims.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreditEntry {
    pub action_id: uuid::Uuid,
    /// Normalized weight (0.0–1.0) — this action's share of the
    /// attributed credit.
    pub credit_weight: f64,
    /// The credited incremental Y14 fans for this action.
    pub credited_incremental_y14: f64,
    /// The credited incremental Y30 fans, if available.
    pub credited_incremental_y30: Option<f64>,
    /// 0.0–1.0, confidence in this action's attribution.
    pub attribution_confidence: f64,
    /// Whether this credit entry is backed by causal evidence (true
    /// only when the experiment assignment's final_evidence_quality =
    /// 'randomized_holdout' and final_contamination < 0.1). False for
    /// proportional attribution. The learner must distinguish
    /// attribution from causal evidence.
    ///
    /// NOTE: This field is currently always `false` — the attribution
    /// worker does not yet set it to `true`. It is a forward-compatible
    /// contract that will be wired when the attribution worker gains
    /// access to experiment assignment quality metadata. Until then,
    /// all credit entries carry `ModeledAttribution` evidence quality.
    #[serde(default)]
    pub is_causal_evidence: bool,
}

/// The attribution method used to allocate credit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionMethod {
    /// Proportional allocation based on observable facts with bounded
    /// prior influence.
    Proportional,
}

impl AttributionMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proportional => "proportional",
        }
    }
}

/// A credit allocator — the trait that the infra layer implements.
/// The brain provides `ProportionalCreditAllocator` as the default.
pub trait CreditAllocator: Send + Sync {
    fn allocate(
        &self,
        outcome: &FanOutcome,
        competing_actions: &[ActionExposure],
    ) -> AttributionResult;
}

/// Proportional credit allocator with anti-self-reinforcement design.
///
/// Weight is driven **primarily** by observable facts, not model predictions:
/// ```text
/// w = exposure_delivered * temporal_proximity * audience_match
///     * attribution_confidence
///     * (1.0 + BOUNDED_PRIOR_WEIGHT * tanh(treatment_effect_prior))
/// ```
/// where `BOUNDED_PRIOR_WEIGHT = 0.2` — the prior can nudge credit
/// allocation by at most ~20%, never dominate it.
///
/// GENUINE RESIDUAL: The total attribution mass is bounded by the mean
/// `attribution_confidence` of the competing actions. When confidence is
/// low, a genuine residual is preserved — "we don't know" is a valid
/// outcome, and no forced 100% attribution is applied. Only when all
/// actions have confidence = 1.0 does the residual collapse to 0.
///
/// `total_attribution_mass = mean(attribution_confidence for w > 0).min(1.0)`
/// `credited_y14 = normalized_weight * total_attribution_mass * observed`
/// `unattributed = observed - sum(credited_y14)`
pub struct ProportionalCreditAllocator;

impl CreditAllocator for ProportionalCreditAllocator {
    fn allocate(
        &self,
        outcome: &FanOutcome,
        competing_actions: &[ActionExposure],
    ) -> AttributionResult {
        // Compute raw weights for each action.
        let weights: Vec<f64> = competing_actions
            .iter()
            .map(|a| {
                if !a.exposure_delivered {
                    return 0.0;
                }
                let observable = a.temporal_proximity * a.audience_match * a.attribution_confidence;
                // Bounded prior: tanh squashes to [-1, 1], then scaled by
                // BOUNDED_PRIOR_WEIGHT (0.2). The prior can nudge the weight
                // by at most ~20%, never dominate it.
                let prior_nudge = 1.0 + BOUNDED_PRIOR_WEIGHT * a.treatment_effect_prior.tanh();
                observable * prior_nudge
            })
            .collect();

        let total_weight: f64 = weights.iter().sum();
        if total_weight <= 0.0 || !total_weight.is_finite() {
            // No actions have positive weight — no forced attribution.
            return AttributionResult {
                credits: Vec::new(),
                unattributed: outcome.observed_incremental_fans,
                method: AttributionMethod::Proportional,
            };
        }

        // GENUINE RESIDUAL: The total attribution mass is bounded by the
        // mean attribution_confidence of actions with positive weight.
        // When confidence is low, a genuine residual is preserved.
        // This prevents the system from always giving 100% to somebody.
        let positive_actions: Vec<&ActionExposure> = competing_actions
            .iter()
            .zip(weights.iter())
            .filter(|&(_, &w)| w > 0.0)
            .map(|(a, _)| a)
            .collect();
        let mean_confidence: f64 = if positive_actions.is_empty() {
            0.0
        } else {
            positive_actions
                .iter()
                .map(|a| a.attribution_confidence)
                .sum::<f64>()
                / positive_actions.len() as f64
        };
        let total_attribution_mass = mean_confidence.min(1.0);

        let mut credits = Vec::with_capacity(competing_actions.len());
        let mut total_credited_y14 = 0.0_f64;
        for (i, action) in competing_actions.iter().enumerate() {
            let weight = weights[i];
            if weight <= 0.0 {
                continue;
            }
            let normalized = weight / total_weight;
            // Scale by total_attribution_mass to preserve a genuine
            // residual when confidence is low.
            let credited_y14 =
                normalized * total_attribution_mass * outcome.observed_incremental_fans;
            let credited_y30 = outcome
                .durable_fans_30d
                .map(|y30| normalized * total_attribution_mass * y30);
            total_credited_y14 += credited_y14;
            credits.push(CreditEntry {
                action_id: action.action_id,
                credit_weight: normalized * total_attribution_mass,
                credited_incremental_y14: credited_y14,
                credited_incremental_y30: credited_y30,
                attribution_confidence: action.attribution_confidence,
                // Proportional attribution is NOT causal evidence.
                // The attribution worker sets this to true only when
                // the experiment assignment's final_evidence_quality
                // = 'randomized_holdout' and final_contamination < 0.1.
                is_causal_evidence: false,
            });
        }

        let unattributed = outcome.observed_incremental_fans - total_credited_y14;
        AttributionResult {
            credits,
            unattributed: unattributed.max(0.0),
            method: AttributionMethod::Proportional,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_action(
        id: uuid::Uuid,
        exposure: bool,
        temporal: f64,
        audience: f64,
        confidence: f64,
        prior: f64,
    ) -> ActionExposure {
        ActionExposure {
            action_id: id,
            template_id: "reddit-scanner".to_string(),
            audience_key: "r/metal".to_string(),
            exposure_delivered: exposure,
            temporal_proximity: temporal,
            audience_match: audience,
            attribution_confidence: confidence,
            treatment_effect_prior: prior,
            evidence_quality: EvidenceQuality::Observational,
        }
    }

    fn make_outcome(fans: f64) -> FanOutcome {
        FanOutcome {
            workspace_id: uuid::Uuid::nil(),
            observed_incremental_fans: fans,
            durable_fans_30d: None,
            measurement_window_start: OffsetDateTime::now_utc(),
            measurement_window_end: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn proportional_split_with_two_competing_actions() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        let actions = [
            make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 1.0, 0.0),
            make_action(uuid::Uuid::now_v7(), true, 0.5, 1.0, 1.0, 0.0),
        ];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 2);
        // First action has 2× the weight of the second (1.0 vs 0.5).
        let total: f64 = result
            .credits
            .iter()
            .map(|c| c.credited_incremental_y14)
            .sum();
        assert!((total - 10.0).abs() < 0.001, "total credited = {total}");
        assert!(
            result.credits[0].credited_incremental_y14 > result.credits[1].credited_incremental_y14
        );
    }

    #[test]
    fn unattributed_residual_preserved() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        // Single action with 0.5 confidence — gets partial credit.
        // GENUINE RESIDUAL: total_attribution_mass = 0.5, so only 50%
        // of the outcome is attributed, and 50% is unattributed.
        let actions = [make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 0.5, 0.0)];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 1);
        // 0.5 attribution mass × 10 = 5 fans credited.
        assert!(
            (result.credits[0].credited_incremental_y14 - 5.0).abs() < 0.001,
            "credited = {}",
            result.credits[0].credited_incremental_y14
        );
        // Genuine residual: 10 - 5 = 5 fans unattributed.
        assert!(
            (result.unattributed - 5.0).abs() < 0.001,
            "unattributed = {}",
            result.unattributed
        );
    }

    #[test]
    fn full_confidence_means_no_residual() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        // Single action with confidence=1.0 → 100% attributed, 0 residual.
        let actions = [make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 1.0, 0.0)];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 1);
        assert!((result.credits[0].credited_incremental_y14 - 10.0).abs() < 0.001);
        assert!(
            result.unattributed < 0.001,
            "unattributed = {}",
            result.unattributed
        );
    }

    #[test]
    fn low_confidence_leaves_genuine_residual() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        // Single action with confidence=0.3 → 30% attributed, 70% residual.
        let actions = [make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 0.3, 0.0)];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 1);
        assert!(
            (result.credits[0].credited_incremental_y14 - 3.0).abs() < 0.001,
            "credited = {}",
            result.credits[0].credited_incremental_y14
        );
        assert!(
            (result.unattributed - 7.0).abs() < 0.001,
            "unattributed = {}",
            result.unattributed
        );
    }

    #[test]
    fn zero_credit_for_no_exposure() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        let actions = [
            make_action(uuid::Uuid::now_v7(), false, 1.0, 1.0, 1.0, 0.0),
            make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 1.0, 0.0),
        ];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 1);
        // Only the exposed action gets credit. confidence=1.0 → full attribution.
        assert!((result.credits[0].credited_incremental_y14 - 10.0).abs() < 0.001);
    }

    #[test]
    fn posterior_prior_has_bounded_influence() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        // Two actions with identical observable facts but very different priors.
        // The prior should only nudge by ~20%, not dominate.
        let actions = [
            make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 1.0, 10.0), // high prior
            make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 1.0, -10.0), // low prior
        ];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 2);
        // With bounded prior, the high-prior action gets slightly more credit,
        // but not dramatically more. tanh(10) ≈ 1.0, tanh(-10) ≈ -1.0.
        // w_high = 1.0 * (1 + 0.2*1) = 1.2
        // w_low  = 1.0 * (1 + 0.2*(-1)) = 0.8
        // mean_confidence = 1.0 → total_attribution_mass = 1.0
        // share_high = 1.2 / 2.0 * 1.0 * 10 = 6 fans
        // share_low  = 0.8 / 2.0 * 1.0 * 10 = 4 fans
        assert!((result.credits[0].credited_incremental_y14 - 6.0).abs() < 0.01);
        assert!((result.credits[1].credited_incremental_y14 - 4.0).abs() < 0.01);
    }

    #[test]
    fn no_actions_means_all_unattributed() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        let result = allocator.allocate(&outcome, &[]);
        assert!(result.credits.is_empty());
        assert!((result.unattributed - 10.0).abs() < 0.001);
    }

    #[test]
    fn all_zero_weight_means_all_unattributed() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        // All actions have zero exposure.
        let actions = [
            make_action(uuid::Uuid::now_v7(), false, 1.0, 1.0, 1.0, 0.0),
            make_action(uuid::Uuid::now_v7(), false, 1.0, 1.0, 1.0, 0.0),
        ];
        let result = allocator.allocate(&outcome, &actions);
        assert!(result.credits.is_empty());
        assert!((result.unattributed - 10.0).abs() < 0.001);
    }

    // ── T6: genuine residual with 0.6 confidence ──

    #[test]
    fn genuine_residual_with_06_confidence() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        // Single action with 0.6 confidence — gets 60% credit.
        // GENUINE RESIDUAL: 6 fans attributed, 4 fans unattributed.
        let actions = [make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 0.6, 0.0)];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 1);
        assert!(
            (result.credits[0].credited_incremental_y14 - 6.0).abs() < 0.001,
            "credited = {}",
            result.credits[0].credited_incremental_y14
        );
        assert!(
            (result.unattributed - 4.0).abs() < 0.001,
            "unattributed = {}",
            result.unattributed
        );
    }

    #[test]
    fn genuine_residual_with_two_actions_weighted() {
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        // Two actions: A=0.7 confidence, B=0.5 confidence.
        // mean_confidence = 0.6 → total_attribution_mass = 0.6.
        // 6 fans attributed, 4 fans unattributed.
        // Split by weight: A has temporal=1.0, B has temporal=0.5.
        // w_A = 1.0 * 0.7 = 0.7, w_B = 0.5 * 0.5 = 0.25
        // share_A = 0.7 / 0.95 * 0.6 * 10 ≈ 4.42
        // share_B = 0.25 / 0.95 * 0.6 * 10 ≈ 1.58
        // total_attributed = 4.42 + 1.58 = 6.0
        // unattributed = 10 - 6 = 4.0
        let actions = [
            make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 0.7, 0.0),
            make_action(uuid::Uuid::now_v7(), true, 0.5, 1.0, 0.5, 0.0),
        ];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 2);
        let total_credited: f64 = result
            .credits
            .iter()
            .map(|c| c.credited_incremental_y14)
            .sum();
        assert!(
            (total_credited - 6.0).abs() < 0.01,
            "total credited = {total_credited}"
        );
        assert!(
            (result.unattributed - 4.0).abs() < 0.01,
            "unattributed = {}",
            result.unattributed
        );
    }

    // ── T8: causal estimator only consumes valid causal evidence ──

    #[test]
    fn proportional_allocator_never_sets_causal_evidence() {
        // The ProportionalCreditAllocator always returns
        // is_causal_evidence = false. The attribution worker sets this
        // flag to true only when the experiment assignment's
        // final_evidence_quality = 'randomized_holdout' and
        // final_contamination < 0.1. The allocator itself never makes
        // that determination.
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        let actions = [
            make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 1.0, 0.0),
            make_action(uuid::Uuid::now_v7(), true, 0.5, 1.0, 0.8, 0.0),
        ];
        let result = allocator.allocate(&outcome, &actions);
        for credit in &result.credits {
            assert!(
                !credit.is_causal_evidence,
                "credit for action {} has is_causal_evidence=true",
                credit.action_id
            );
        }
    }

    #[test]
    fn observational_evidence_never_becomes_causal() {
        // Even with full confidence and perfect exposure, observational
        // evidence (no randomized holdout) never produces causal evidence.
        // The evidence_quality field on the action exposure is
        // Observational — the allocator must respect that.
        let allocator = ProportionalCreditAllocator;
        let outcome = make_outcome(10.0);
        let actions = [make_action(uuid::Uuid::now_v7(), true, 1.0, 1.0, 1.0, 0.0)];
        let result = allocator.allocate(&outcome, &actions);
        assert_eq!(result.credits.len(), 1);
        assert!(!result.credits[0].is_causal_evidence);
    }

    #[test]
    fn observational_evidence_leaves_most_of_the_outcome_unattributed() {
        // The residual is what "we don't know" looks like. Attribution mass
        // is bounded by mean confidence, and confidence is now the evidence
        // quality — so an action with no causal claim on a fan cannot take
        // credit for one. It used to be a flat 0.7 for every action, which
        // made the residual exactly 30% whatever the evidence said.
        let outcome = make_outcome(10.0);
        let weak = make_action(
            uuid::Uuid::from_u128(1),
            true,
            1.0,
            1.0,
            EvidenceQuality::Observational.weight(),
            0.0,
        );
        let result = ProportionalCreditAllocator.allocate(&outcome, &[weak]);
        let credited: f64 = result
            .credits
            .iter()
            .map(|c| c.credited_incremental_y14)
            .sum();
        assert!(
            (credited - 1.0).abs() < 1e-9,
            "observational evidence claims a tenth, not 70%: {credited}"
        );
        assert!((result.unattributed - 9.0).abs() < 1e-9);
    }

    #[test]
    fn a_randomized_holdout_claims_the_whole_outcome() {
        let outcome = make_outcome(10.0);
        let strong = make_action(
            uuid::Uuid::from_u128(1),
            true,
            1.0,
            1.0,
            EvidenceQuality::RandomizedHoldout.weight(),
            0.0,
        );
        let result = ProportionalCreditAllocator.allocate(&outcome, &[strong]);
        let credited: f64 = result
            .credits
            .iter()
            .map(|c| c.credited_incremental_y14)
            .sum();
        assert!((credited - 10.0).abs() < 1e-9, "{credited}");
        assert!(result.unattributed.abs() < 1e-9);
    }

    #[test]
    fn two_actions_of_unequal_evidence_still_leave_a_residual() {
        let outcome = make_outcome(10.0);
        let strong = make_action(
            uuid::Uuid::from_u128(1),
            true,
            1.0,
            1.0,
            EvidenceQuality::RandomizedHoldout.weight(),
            0.0,
        );
        let weak = make_action(
            uuid::Uuid::from_u128(2),
            true,
            1.0,
            1.0,
            EvidenceQuality::Observational.weight(),
            0.0,
        );
        let result = ProportionalCreditAllocator.allocate(&outcome, &[strong, weak]);
        let credit_of = |id: u128| {
            result
                .credits
                .iter()
                .find(|c| c.action_id == uuid::Uuid::from_u128(id))
                .map(|c| c.credited_incremental_y14)
                .expect("credit")
        };
        assert!(
            credit_of(1) > credit_of(2),
            "stronger evidence must take the larger share: {} vs {}",
            credit_of(1),
            credit_of(2)
        );
        // Mean confidence is (1.0 + 0.1) / 2 = 0.55, so 45% stays unknown.
        assert!(
            (result.unattributed - 4.5).abs() < 1e-9,
            "{}",
            result.unattributed
        );
    }
}
