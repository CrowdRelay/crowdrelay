//! Performance-learning bounded context for delayed Autopilot effect measurements.
//!
//! The context does not decide which SQL metric to query. It receives one baseline
//! and one later observation and classifies the effect deterministically.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectDirection {
    HigherIsBetter,
    LowerIsBetter,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectAssessment {
    Improved,
    Neutral,
    Worsened,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct EffectResult {
    pub assessment: EffectAssessment,
    pub delta_basis_points: i32,
}

/// Relative effect with a neutral band to avoid interpreting ordinary noise as learning.
#[must_use]
pub fn assess_effect(
    baseline: f64,
    observed: f64,
    direction: EffectDirection,
    neutral_band_basis_points: u16,
) -> Option<EffectResult> {
    if !baseline.is_finite()
        || !observed.is_finite()
        || baseline < 0.0
        || observed < 0.0
        || neutral_band_basis_points > 10_000
    {
        return None;
    }
    let delta = if baseline == 0.0 {
        if observed == 0.0 { 0 } else { 10_000 }
    } else {
        let raw = ((observed - baseline) / baseline) * 10_000.0;
        raw.clamp(f64::from(i32::MIN), f64::from(i32::MAX)).round() as i32
    };
    let signed = match direction {
        EffectDirection::HigherIsBetter => delta,
        EffectDirection::LowerIsBetter => delta.saturating_neg(),
    };
    let band = i32::from(neutral_band_basis_points);
    let assessment = if signed > band {
        EffectAssessment::Improved
    } else if signed < -band {
        EffectAssessment::Worsened
    } else {
        EffectAssessment::Neutral
    };
    Some(EffectResult {
        assessment,
        delta_basis_points: delta,
    })
}

/// Classifies an effect that has already had its counterfactual subtracted.
///
/// [`assess_effect`] compares a level against a baseline and refuses a
/// negative observation, because a negative *level* — minus four ticket sales,
/// minus two push endpoints — is a malformed reading rather than a result. A
/// difference-in-differences estimate is not a level. It is the subtraction
/// itself, and its sign is the answer: a negative one says the action did
/// worse than doing nothing, which is precisely the thing the learner has to
/// be able to find out.
///
/// So signed effects get their own entry point rather than a relaxed flag on
/// the level-based one. The two carry different meanings for the same number
/// and the type system should not let a caller confuse them.
///
/// `counterfactual` is the magnitude the effect is expressed against and is
/// used only to render the relative delta. The assessment itself compares the
/// effect to zero, because zero is what "the action changed nothing" means
/// here.
#[must_use]
pub fn assess_signed_effect(
    counterfactual: f64,
    observed_effect: f64,
    neutral_band_basis_points: u16,
) -> Option<EffectResult> {
    if !counterfactual.is_finite()
        || !observed_effect.is_finite()
        || counterfactual < 0.0
        || neutral_band_basis_points > 10_000
    {
        return None;
    }
    let delta = if counterfactual == 0.0 {
        // Nothing to express the effect against. The sign still carries the
        // finding, so saturate rather than discard it.
        if observed_effect == 0.0 {
            0
        } else if observed_effect > 0.0 {
            10_000
        } else {
            -10_000
        }
    } else {
        let raw = (observed_effect / counterfactual) * 10_000.0;
        raw.clamp(f64::from(i32::MIN), f64::from(i32::MAX)).round() as i32
    };
    let band = i32::from(neutral_band_basis_points);
    let assessment = if delta > band {
        EffectAssessment::Improved
    } else if delta < -band {
        EffectAssessment::Worsened
    } else {
        EffectAssessment::Neutral
    };
    Some(EffectResult {
        assessment,
        delta_basis_points: delta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_effect_accepts_a_negative_result_as_a_finding() {
        // The whole point: an action that did worse than nothing must produce
        // a classified result, not an absent one.
        let result = assess_signed_effect(4.0, -2.0, 500);
        assert_eq!(
            result.map(|value| value.assessment),
            Some(EffectAssessment::Worsened)
        );
        assert_eq!(result.map(|value| value.delta_basis_points), Some(-5_000));
    }

    #[test]
    fn level_based_assessment_still_refuses_a_negative_level() {
        // And the level-based path keeps refusing it, so the distinction
        // between the two entry points stays meaningful.
        assert!(assess_effect(4.0, -2.0, EffectDirection::HigherIsBetter, 500).is_none());
    }

    #[test]
    fn signed_effect_without_a_counterfactual_keeps_the_sign() {
        assert_eq!(
            assess_signed_effect(0.0, -3.0, 500).map(|value| value.assessment),
            Some(EffectAssessment::Worsened)
        );
        assert_eq!(
            assess_signed_effect(0.0, 3.0, 500).map(|value| value.assessment),
            Some(EffectAssessment::Improved)
        );
        assert_eq!(
            assess_signed_effect(0.0, 0.0, 500).map(|value| value.assessment),
            Some(EffectAssessment::Neutral)
        );
    }

    #[test]
    fn signed_effect_inside_the_band_is_neutral() {
        assert_eq!(
            assess_signed_effect(100.0, 2.0, 500).map(|value| value.assessment),
            Some(EffectAssessment::Neutral)
        );
    }

    #[test]
    fn material_growth_is_improvement_when_higher_is_better() {
        let result = assess_effect(100.0, 125.0, EffectDirection::HigherIsBetter, 500);
        assert_eq!(
            result.map(|value| value.assessment),
            Some(EffectAssessment::Improved)
        );
        assert_eq!(result.map(|value| value.delta_basis_points), Some(2_500));
    }

    #[test]
    fn neutral_band_rejects_noise() {
        let result = assess_effect(100.0, 102.0, EffectDirection::HigherIsBetter, 500);
        assert_eq!(
            result.map(|value| value.assessment),
            Some(EffectAssessment::Neutral)
        );
    }

    #[test]
    fn lower_is_better_flips_interpretation_without_flipping_evidence_delta() {
        let result = assess_effect(100.0, 80.0, EffectDirection::LowerIsBetter, 500);
        assert_eq!(
            result.map(|value| value.assessment),
            Some(EffectAssessment::Improved)
        );
        assert_eq!(result.map(|value| value.delta_basis_points), Some(-2_000));
    }

    #[test]
    fn malformed_measurement_is_not_classified() {
        assert!(assess_effect(-1.0, 1.0, EffectDirection::HigherIsBetter, 500).is_none());
        assert!(assess_effect(1.0, f64::NAN, EffectDirection::HigherIsBetter, 500).is_none());
    }
}
