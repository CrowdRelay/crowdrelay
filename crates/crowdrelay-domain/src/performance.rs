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

#[cfg(test)]
mod tests {
    use super::*;

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
