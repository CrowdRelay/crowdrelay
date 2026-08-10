//! Deterministic experiment-optimization bounded context.
//!
//! The domain works only on aggregate observations and integer arithmetic.
//! It deliberately has no RNG, async runtime, database, provider SDK or ML
//! dependency. Exploration is a small bounded bonus for under-observed variants;
//! assignment uses a stable FNV-1a bucket so the same subject remains in the
//! same variant while an allocation version is unchanged.

use serde::{Deserialize, Serialize};

use crate::{ExperimentId, ExperimentVariantId, autonomy::Confidence};

const BASIS_POINTS: u128 = 10_000;
const MAX_EXPLORATION_BONUS_BASIS_POINTS: u32 = 1_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentMetric {
    Conversion,
    RevenuePerExposure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ExperimentVariantSnapshot {
    pub variant_id: ExperimentVariantId,
    pub exposures: u64,
    pub conversions: u64,
    pub value_minor: i64,
    pub allocation_basis_points: u16,
    pub active: bool,
}

/// Minimal stable allocation view used when assigning a subject to a variant.
/// It deliberately excludes outcome counters so assignment cannot accidentally
/// depend on mutable performance data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ExperimentAllocationSlot {
    pub variant_id: ExperimentVariantId,
    pub allocation_basis_points: u16,
    pub active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExperimentSnapshot {
    pub experiment_id: ExperimentId,
    pub version: i64,
    pub metric: ExperimentMetric,
    pub running: bool,
    pub variants: Vec<ExperimentVariantSnapshot>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExperimentPolicy {
    pub minimum_samples_per_variant: u64,
    pub maximum_allocation_step_basis_points: u16,
    pub winner_margin_basis_points: u16,
    pub winner_minimum_total_samples: u64,
}

impl Default for ExperimentPolicy {
    fn default() -> Self {
        Self {
            minimum_samples_per_variant: 30,
            maximum_allocation_step_basis_points: 1_000,
            winner_margin_basis_points: 1_000,
            winner_minimum_total_samples: 200,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExperimentDecision {
    Hold(ExperimentHoldReason),
    Reallocate {
        winner: ExperimentVariantId,
        allocations: Vec<(ExperimentVariantId, u16)>,
        confidence: Confidence,
    },
    Complete {
        winner: ExperimentVariantId,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExperimentHoldReason {
    NotRunning,
    InvalidPolicy,
    InvalidExperiment,
    InsufficientSamples,
    NoMaterialDifference,
}

/// Evaluates aggregate experiment evidence using only deterministic integer
/// arithmetic. Revenue is normalized relative to the best observed
/// revenue-per-exposure ratio before the exploration bonus is applied, so its
/// currency scale cannot drown out exploration as it could with a raw float
/// score.
#[must_use]
pub fn evaluate_experiment(
    snapshot: &ExperimentSnapshot,
    policy: ExperimentPolicy,
) -> ExperimentDecision {
    if !snapshot.running {
        return ExperimentDecision::Hold(ExperimentHoldReason::NotRunning);
    }
    if !valid_policy(policy) {
        return ExperimentDecision::Hold(ExperimentHoldReason::InvalidPolicy);
    }

    let active = snapshot
        .variants
        .iter()
        .filter(|variant| variant.active)
        .collect::<Vec<_>>();
    if !valid_experiment(snapshot.version, &active) {
        return ExperimentDecision::Hold(ExperimentHoldReason::InvalidExperiment);
    }
    if active
        .iter()
        .any(|variant| variant.exposures < policy.minimum_samples_per_variant)
    {
        return ExperimentDecision::Hold(ExperimentHoldReason::InsufficientSamples);
    }

    let total_samples = active
        .iter()
        .try_fold(0_u64, |total, variant| total.checked_add(variant.exposures));
    let Some(total_samples) = total_samples else {
        return ExperimentDecision::Hold(ExperimentHoldReason::InvalidExperiment);
    };
    let maximum_exposures = active
        .iter()
        .map(|variant| variant.exposures)
        .max()
        .unwrap_or(1);
    let revenue_reference = if matches!(snapshot.metric, ExperimentMetric::RevenuePerExposure) {
        best_revenue_variant(&active)
    } else {
        None
    };

    let mut scored = active
        .iter()
        .map(|variant| {
            let performance = performance_basis_points(snapshot.metric, variant, revenue_reference);
            let exploration = exploration_bonus_basis_points(variant.exposures, maximum_exposures);
            (
                **variant,
                u32::from(performance).saturating_add(exploration),
            )
        })
        .collect::<Vec<_>>();
    scored.sort_by(|(left_variant, left_score), (right_variant, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| right_variant.exposures.cmp(&left_variant.exposures))
            .then_with(|| left_variant.variant_id.cmp(&right_variant.variant_id))
    });

    let Some((winner, winner_score)) = scored.first().copied() else {
        return ExperimentDecision::Hold(ExperimentHoldReason::InvalidExperiment);
    };
    let runner_score = scored.get(1).map_or(winner_score, |(_, score)| *score);
    let margin_basis_points = score_margin_basis_points(winner_score, runner_score);
    if margin_basis_points < policy.winner_margin_basis_points {
        return ExperimentDecision::Hold(ExperimentHoldReason::NoMaterialDifference);
    }

    let confidence = Confidence::saturating_from_basis_points(
        8_000_u16.saturating_add(margin_basis_points.min(2_000)),
    );
    if total_samples >= policy.winner_minimum_total_samples
        && margin_basis_points >= policy.winner_margin_basis_points.saturating_mul(2)
    {
        return ExperimentDecision::Complete {
            winner: winner.variant_id,
            confidence,
        };
    }

    ExperimentDecision::Reallocate {
        winner: winner.variant_id,
        allocations: shift_allocation(
            &active,
            winner.variant_id,
            policy.maximum_allocation_step_basis_points,
        ),
        confidence,
    }
}

/// Stable deterministic assignment for provider/application boundaries. The
/// caller supplies a non-PII stable key (for example a fan UUID's bytes). No
/// random dependency is required, and input variant order does not influence
/// the result.
#[must_use]
pub fn assign_variant(
    experiment_id: ExperimentId,
    assignment_key: &[u8],
    variants: &[ExperimentAllocationSlot],
) -> Option<ExperimentVariantId> {
    let mut active = variants
        .iter()
        .filter(|variant| variant.active && variant.allocation_basis_points > 0)
        .collect::<Vec<_>>();
    if active.is_empty()
        || active
            .iter()
            .map(|variant| u32::from(variant.allocation_basis_points))
            .sum::<u32>()
            != 10_000
    {
        return None;
    }
    active.sort_by_key(|variant| variant.variant_id);

    let mut hash = 14_695_981_039_346_656_037_u64;
    for byte in experiment_id
        .as_uuid()
        .as_bytes()
        .iter()
        .copied()
        .chain(assignment_key.iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    let bucket = u32::try_from(hash % 10_000).ok()?;
    let mut cumulative = 0_u32;
    for variant in active {
        cumulative = cumulative.saturating_add(u32::from(variant.allocation_basis_points));
        if bucket < cumulative {
            return Some(variant.variant_id);
        }
    }
    None
}

const fn valid_policy(policy: ExperimentPolicy) -> bool {
    policy.minimum_samples_per_variant > 0
        && policy.maximum_allocation_step_basis_points > 0
        && policy.maximum_allocation_step_basis_points <= 2_500
        && policy.winner_margin_basis_points <= 10_000
        && policy.winner_minimum_total_samples > 0
}

fn valid_experiment(version: i64, active: &[&ExperimentVariantSnapshot]) -> bool {
    version > 0
        && active.len() >= 2
        && active
            .iter()
            .map(|variant| u32::from(variant.allocation_basis_points))
            .sum::<u32>()
            == 10_000
        && active.iter().all(|variant| {
            variant.exposures > 0
                && variant.conversions <= variant.exposures
                && variant.value_minor >= 0
        })
}

fn best_revenue_variant<'a>(
    active: &'a [&ExperimentVariantSnapshot],
) -> Option<&'a ExperimentVariantSnapshot> {
    active.iter().copied().max_by(|left, right| {
        let left_value = u128::try_from(left.value_minor).unwrap_or(0);
        let right_value = u128::try_from(right.value_minor).unwrap_or(0);
        left_value
            .saturating_mul(u128::from(right.exposures))
            .cmp(&right_value.saturating_mul(u128::from(left.exposures)))
            .then_with(|| right.variant_id.cmp(&left.variant_id))
    })
}

fn performance_basis_points(
    metric: ExperimentMetric,
    variant: &ExperimentVariantSnapshot,
    revenue_reference: Option<&ExperimentVariantSnapshot>,
) -> u16 {
    match metric {
        ExperimentMetric::Conversion => ratio_basis_points(variant.conversions, variant.exposures),
        ExperimentMetric::RevenuePerExposure => {
            let Some(reference) = revenue_reference else {
                return 0;
            };
            let Ok(value) = u128::try_from(variant.value_minor) else {
                return 0;
            };
            let Ok(reference_value) = u128::try_from(reference.value_minor) else {
                return 0;
            };
            if reference_value == 0 {
                return 0;
            }
            let numerator = value
                .saturating_mul(u128::from(reference.exposures))
                .saturating_mul(BASIS_POINTS);
            let denominator = u128::from(variant.exposures).saturating_mul(reference_value);
            if denominator == 0 {
                return 0;
            }
            u16::try_from((numerator / denominator).min(BASIS_POINTS)).unwrap_or(10_000)
        }
    }
}

fn ratio_basis_points(numerator: u64, denominator: u64) -> u16 {
    if denominator == 0 {
        return 0;
    }
    u16::try_from(
        u128::from(numerator)
            .saturating_mul(BASIS_POINTS)
            .checked_div(u128::from(denominator))
            .unwrap_or(0)
            .min(BASIS_POINTS),
    )
    .unwrap_or(10_000)
}

fn exploration_bonus_basis_points(exposures: u64, maximum_exposures: u64) -> u32 {
    if maximum_exposures == 0 || exposures >= maximum_exposures {
        return 0;
    }
    let deficit = maximum_exposures.saturating_sub(exposures);
    u32::try_from(
        u128::from(deficit).saturating_mul(u128::from(MAX_EXPLORATION_BONUS_BASIS_POINTS))
            / u128::from(maximum_exposures),
    )
    .unwrap_or(MAX_EXPLORATION_BONUS_BASIS_POINTS)
    .min(MAX_EXPLORATION_BONUS_BASIS_POINTS)
}

fn score_margin_basis_points(winner: u32, runner: u32) -> u16 {
    if winner == 0 || winner <= runner {
        return 0;
    }
    u16::try_from(
        u64::from(winner.saturating_sub(runner))
            .saturating_mul(10_000)
            .checked_div(u64::from(winner))
            .unwrap_or(0)
            .min(10_000),
    )
    .unwrap_or(10_000)
}

fn shift_allocation(
    active: &[&ExperimentVariantSnapshot],
    winner: ExperimentVariantId,
    step: u16,
) -> Vec<(ExperimentVariantId, u16)> {
    let losers = active.len().saturating_sub(1) as u32;
    if losers == 0 {
        return active
            .iter()
            .map(|variant| (variant.variant_id, variant.allocation_basis_points))
            .collect();
    }
    let winner_current = active
        .iter()
        .find(|variant| variant.variant_id == winner)
        .map_or(0, |variant| variant.allocation_basis_points);
    let available = active
        .iter()
        .filter(|variant| variant.variant_id != winner)
        .map(|variant| u32::from(variant.allocation_basis_points.saturating_sub(500)))
        .sum::<u32>();
    let shift = u32::from(step)
        .min(available)
        .min(10_000_u32.saturating_sub(u32::from(winner_current)));
    let base = shift / losers;
    let remainder = shift % losers;
    let mut loser_index = 0_u32;

    active
        .iter()
        .map(|variant| {
            if variant.variant_id == winner {
                (
                    variant.variant_id,
                    u16::try_from(u32::from(variant.allocation_basis_points).saturating_add(shift))
                        .unwrap_or(10_000),
                )
            } else {
                let take = base + u32::from(loser_index < remainder);
                loser_index = loser_index.saturating_add(1);
                (
                    variant.variant_id,
                    variant.allocation_basis_points.saturating_sub(
                        u16::try_from(take).unwrap_or(variant.allocation_basis_points),
                    ),
                )
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(
        exposures: u64,
        conversions: u64,
        value_minor: i64,
        allocation_basis_points: u16,
    ) -> ExperimentVariantSnapshot {
        ExperimentVariantSnapshot {
            variant_id: ExperimentVariantId::new(),
            exposures,
            conversions,
            value_minor,
            allocation_basis_points,
            active: true,
        }
    }

    #[test]
    fn insufficient_samples_never_reallocates() {
        let snapshot = ExperimentSnapshot {
            experiment_id: ExperimentId::new(),
            version: 1,
            metric: ExperimentMetric::Conversion,
            running: true,
            variants: vec![variant(10, 5, 0, 5_000), variant(10, 1, 0, 5_000)],
        };
        assert_eq!(
            evaluate_experiment(&snapshot, ExperimentPolicy::default()),
            ExperimentDecision::Hold(ExperimentHoldReason::InsufficientSamples),
        );
    }

    #[test]
    fn material_conversion_winner_gets_bounded_more_traffic() {
        let winner = variant(100, 60, 0, 5_000);
        let loser = variant(100, 10, 0, 5_000);
        let snapshot = ExperimentSnapshot {
            experiment_id: ExperimentId::new(),
            version: 1,
            metric: ExperimentMetric::Conversion,
            running: true,
            variants: vec![winner, loser],
        };
        assert!(matches!(
            evaluate_experiment(
                &snapshot,
                ExperimentPolicy {
                    winner_minimum_total_samples: 1_000,
                    ..ExperimentPolicy::default()
                },
            ),
            ExperimentDecision::Reallocate { winner: id, .. } if id == winner.variant_id
        ));
    }

    #[test]
    fn revenue_metric_is_scale_independent() {
        let winner = variant(100, 0, 50_000, 5_000);
        let loser = variant(100, 0, 10_000, 5_000);
        let snapshot = ExperimentSnapshot {
            experiment_id: ExperimentId::new(),
            version: 1,
            metric: ExperimentMetric::RevenuePerExposure,
            running: true,
            variants: vec![winner, loser],
        };
        assert!(matches!(
            evaluate_experiment(
                &snapshot,
                ExperimentPolicy {
                    winner_minimum_total_samples: 1_000,
                    ..ExperimentPolicy::default()
                },
            ),
            ExperimentDecision::Reallocate { winner: id, .. } if id == winner.variant_id
        ));
    }

    #[test]
    fn assignment_is_stable_and_independent_of_input_order() {
        let experiment_id = ExperimentId::new();
        let left = ExperimentAllocationSlot {
            variant_id: ExperimentVariantId::new(),
            allocation_basis_points: 5_000,
            active: true,
        };
        let right = ExperimentAllocationSlot {
            variant_id: ExperimentVariantId::new(),
            allocation_basis_points: 5_000,
            active: true,
        };
        let key = b"stable-subject-key";
        let first = assign_variant(experiment_id, key, &[left, right]);
        let second = assign_variant(experiment_id, key, &[right, left]);
        assert!(first.is_some());
        assert_eq!(first, second);
    }
}
