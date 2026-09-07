//! Walk-forward validation — experimental rigor for fan growth.
//!
//! Ported from Kern's `brain::validation`. A template that looks great
//! in-sample but fails out-of-sample is not promoted to Active. A
//! template that uses information that wasn't available at decision time
//! is invalid, no matter how good its recent results look.
//!
//! # Adaptation from Kern
//!
//! Kern's validation windows are in days (1-day vs 14-day alpha).
//! CrowdRelay's are in days too (Y14 vs Y30), but the purge gap must
//! account for the 30-day durability window — a Y30 observation from
//! day 15 overlaps with a Y14 observation from day 25, so the purge
//! gap must be at least 16 days.
//!
//! # Three pillars
//!
//! 1. **Walk-forward validation**: split evidence into in-sample and
//!    out-of-sample windows with a purge gap. A template must perform
//!    well out-of-sample to earn trust.
//! 2. **Point-in-time integrity**: every observation has timestamps
//!    for when the event happened, when data was ingested, when it was
//!    available, when the brain decided, and when the action executed.
//! 3. **Purged cross-validation**: K-fold CV with purge gaps to
//!    prevent information leakage from adjacent folds.

use serde::{Deserialize, Serialize};

/// A performance record for a template over a validation window.
///
/// Adapted from Kern's `PerformanceRecord`: instead of alpha in bps,
/// CrowdRelay measures incremental fans (Y30 durable fans).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct GrowthPerformanceRecord {
    /// Number of observations in this window.
    pub observations: u32,
    /// Mean incremental fans per dispatch.
    pub mean_fans: f64,
    /// Standard deviation of incremental fans.
    pub std_fans: f64,
}

/// One walk-forward validation window.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationWindow {
    pub in_sample_start: time::OffsetDateTime,
    pub in_sample_end: time::OffsetDateTime,
    /// Purge gap between in-sample and OOS (prevents leakage).
    /// Must be at least 16 days to account for Y30 durability overlap.
    pub purge_days: u32,
    pub out_of_sample_start: time::OffsetDateTime,
    pub out_of_sample_end: time::OffsetDateTime,
}

/// Result of walk-forward validation for one template.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct WalkForwardResult {
    pub in_sample: GrowthPerformanceRecord,
    pub out_of_sample: GrowthPerformanceRecord,
    /// How much performance degraded from in-sample to OOS, in fans.
    /// Positive = OOS worse than in-sample (overfitting or decay).
    pub degradation_fans: f64,
    /// Overfitting score: 0 = no overfitting, 10_000 = severe overfitting.
    pub overfitting_score_bps: u16,
    /// Whether the template passes validation.
    pub passed: bool,
}

/// One point-in-time observation with strict timestamp integrity.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PointInTimeObservation {
    /// When the dispatch event actually happened.
    pub event_time: time::OffsetDateTime,
    /// When the data was ingested into our system.
    pub ingestion_time: time::OffsetDateTime,
    /// When the data was available for decisions.
    pub availability_time: time::OffsetDateTime,
    /// When the brain made a decision based on this data.
    pub decision_time: time::OffsetDateTime,
    /// When the action was actually executed (None if not yet).
    pub execution_time: Option<time::OffsetDateTime>,
    /// The observed incremental fans (Y30).
    pub incremental_fans: f64,
}

/// A point-in-time integrity violation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum PitViolation {
    /// Decision was made before data was available (look-ahead bias).
    DecisionBeforeAvailability,
    /// Ingestion happened before the event (impossible — data fabrication).
    IngestionBeforeEvent,
    /// Execution happened before the decision (impossible).
    ExecutionBeforeDecision,
}

/// Purged and embargoed cross-validation configuration.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PurgedCvConfig {
    /// Number of folds.
    pub k_folds: u32,
    /// Days to purge between folds (no observations in the gap).
    /// Default: 16 (Y30 durability overlap).
    pub purge_days: u32,
    /// Days to embargo after each fold.
    pub embargo_days: u32,
}

impl Default for PurgedCvConfig {
    fn default() -> Self {
        Self {
            k_folds: 5,
            purge_days: 16,
            embargo_days: 7,
        }
    }
}

/// Validates a template using walk-forward analysis.
///
/// Compares in-sample performance to out-of-sample performance.
/// Pass criteria are adaptive to OOS sample count:
/// - < 5 OOS samples: always pass (can't validate meaningfully)
/// - 5-14 OOS samples: OOS mean must be positive
/// - 15+ OOS samples: OOS mean > 0 AND overfitting score < 7_000
#[must_use]
pub fn validate_template(
    in_sample: GrowthPerformanceRecord,
    out_of_sample: GrowthPerformanceRecord,
) -> WalkForwardResult {
    let degradation_fans = in_sample.mean_fans - out_of_sample.mean_fans;

    // Overfitting score: how much worse is OOS vs in-sample?
    let overfitting_score_bps = if in_sample.mean_fans > 0.0 {
        let ratio = (degradation_fans / in_sample.mean_fans) * 10_000.0;
        u16::try_from(ratio.max(0.0) as i64)
            .unwrap_or(10_000)
            .min(10_000)
    } else if out_of_sample.mean_fans <= 0.0 {
        10_000
    } else {
        0
    };

    // Pass criteria — adaptive to sample size.
    let passed = if out_of_sample.observations < 5 {
        true
    } else if out_of_sample.observations < 15 {
        out_of_sample.mean_fans > 0.0
    } else {
        out_of_sample.mean_fans > 0.0 && overfitting_score_bps < 7_000
    };

    WalkForwardResult {
        in_sample,
        out_of_sample,
        degradation_fans,
        overfitting_score_bps,
        passed,
    }
}

/// Checks point-in-time integrity for a set of observations.
///
/// Returns a list of violations. An empty list means all observations
/// are PIT-clean. Any violation invalidates the template's evidence.
#[must_use]
pub fn check_pit_integrity(observations: &[PointInTimeObservation]) -> Vec<(usize, PitViolation)> {
    let mut violations = Vec::new();

    for (i, obs) in observations.iter().enumerate() {
        if obs.decision_time < obs.availability_time {
            violations.push((i, PitViolation::DecisionBeforeAvailability));
        }
        if obs.ingestion_time < obs.event_time {
            violations.push((i, PitViolation::IngestionBeforeEvent));
        }
        if let Some(exec_time) = obs.execution_time
            && exec_time < obs.decision_time
        {
            violations.push((i, PitViolation::ExecutionBeforeDecision));
        }
    }

    violations
}

/// Splits observations into in-sample and out-of-sample windows with
/// a purge gap. Returns `(in_sample, out_of_sample)` records.
///
/// Observations within the purge gap are excluded from both windows.
#[must_use]
pub fn split_walk_forward(
    observations: &[(time::OffsetDateTime, f64)],
    split_date: time::OffsetDateTime,
    purge_days: u32,
) -> (GrowthPerformanceRecord, GrowthPerformanceRecord) {
    let purge_end = split_date + time::Duration::days(purge_days as i64);

    let in_sample: Vec<f64> = observations
        .iter()
        .filter(|(t, _)| *t < split_date)
        .map(|(_, fans)| *fans)
        .collect();
    let out_of_sample: Vec<f64> = observations
        .iter()
        .filter(|(t, _)| *t >= purge_end)
        .map(|(_, fans)| *fans)
        .collect();

    (record_from(&in_sample), record_from(&out_of_sample))
}

/// Computes a `GrowthPerformanceRecord` from a slice of fan counts.
#[must_use]
fn record_from(fans: &[f64]) -> GrowthPerformanceRecord {
    let n = fans.len() as u32;
    if n == 0 {
        return GrowthPerformanceRecord::default();
    }
    let mean = fans.iter().copied().sum::<f64>() / n as f64;
    let variance = if n > 1 {
        fans.iter().map(|f| (f - mean).powi(2)).sum::<f64>() / (n - 1) as f64
    } else {
        0.0
    };
    let std = variance.sqrt();
    GrowthPerformanceRecord {
        observations: n,
        mean_fans: mean,
        std_fans: std,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(day: i32) -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc() + time::Duration::days(day as i64)
    }

    #[test]
    fn validate_passes_with_positive_oos() {
        let in_sample = GrowthPerformanceRecord {
            observations: 20,
            mean_fans: 5.0,
            std_fans: 2.0,
        };
        let oos = GrowthPerformanceRecord {
            observations: 20,
            mean_fans: 3.0,
            std_fans: 1.5,
        };
        let result = validate_template(in_sample, oos);
        assert!(result.passed);
        assert!(result.degradation_fans > 0.0); // some degradation
        assert!(result.overfitting_score_bps < 7_000); // not severe
    }

    #[test]
    fn validate_fails_with_negative_oos() {
        let in_sample = GrowthPerformanceRecord {
            observations: 20,
            mean_fans: 5.0,
            std_fans: 2.0,
        };
        let oos = GrowthPerformanceRecord {
            observations: 20,
            mean_fans: -1.0,
            std_fans: 1.5,
        };
        let result = validate_template(in_sample, oos);
        assert!(!result.passed);
    }

    #[test]
    fn validate_passes_with_low_oos_samples() {
        // < 5 OOS samples: always pass
        let in_sample = GrowthPerformanceRecord {
            observations: 20,
            mean_fans: 5.0,
            std_fans: 2.0,
        };
        let oos = GrowthPerformanceRecord {
            observations: 3,
            mean_fans: -10.0, // even negative
            std_fans: 1.0,
        };
        let result = validate_template(in_sample, oos);
        assert!(result.passed);
    }

    #[test]
    fn validate_fails_with_severe_overfitting() {
        let in_sample = GrowthPerformanceRecord {
            observations: 20,
            mean_fans: 10.0,
            std_fans: 2.0,
        };
        let oos = GrowthPerformanceRecord {
            observations: 20,
            mean_fans: 0.5, // huge degradation
            std_fans: 1.0,
        };
        let result = validate_template(in_sample, oos);
        // OOS > 0 but severe overfitting
        assert!(!result.passed);
        assert!(result.overfitting_score_bps >= 7_000);
    }

    #[test]
    fn pit_check_detects_look_ahead() {
        let obs = vec![PointInTimeObservation {
            event_time: ts(1),
            ingestion_time: ts(2),
            availability_time: ts(3),
            decision_time: ts(2), // before availability!
            execution_time: Some(ts(4)),
            incremental_fans: 1.0,
        }];
        let violations = check_pit_integrity(&obs);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].1, PitViolation::DecisionBeforeAvailability);
    }

    #[test]
    fn pit_check_detects_fabrication() {
        let obs = vec![PointInTimeObservation {
            event_time: ts(5),
            ingestion_time: ts(1), // before event!
            availability_time: ts(6),
            decision_time: ts(7),
            execution_time: Some(ts(8)),
            incremental_fans: 1.0,
        }];
        let violations = check_pit_integrity(&obs);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].1, PitViolation::IngestionBeforeEvent);
    }

    #[test]
    fn pit_check_clean_for_valid_observation() {
        let obs = vec![PointInTimeObservation {
            event_time: ts(1),
            ingestion_time: ts(2),
            availability_time: ts(3),
            decision_time: ts(4),
            execution_time: Some(ts(5)),
            incremental_fans: 1.0,
        }];
        let violations = check_pit_integrity(&obs);
        assert!(violations.is_empty());
    }

    #[test]
    fn split_walk_forward_excludes_purge_gap() {
        let observations = vec![
            (ts(-30), 5.0), // in-sample (before split)
            (ts(-20), 3.0), // in-sample (before split)
            (ts(-5), 4.0),  // in-sample (before split)
            (ts(10), 2.0),  // purge gap (split <= t < purge_end=16)
            (ts(20), 1.0),  // OOS (t >= purge_end=16)
        ];
        let split = ts(0);
        let (in_sample, oos) = split_walk_forward(&observations, split, 16);
        assert_eq!(in_sample.observations, 3); // 3 before split
        assert_eq!(oos.observations, 1); // 1 after purge gap
        // Purge gap observation (ts(10)) excluded from both
        assert!((in_sample.mean_fans - 4.0).abs() < 0.01); // (5+3+4)/3 = 4
        assert!((oos.mean_fans - 1.0).abs() < 0.01);
    }

    #[test]
    fn split_walk_forward_empty_observations() {
        let (in_sample, oos) = split_walk_forward(&[], ts(0), 16);
        assert_eq!(in_sample.observations, 0);
        assert_eq!(oos.observations, 0);
    }

    #[test]
    fn purged_cv_config_default_has_16_day_purge() {
        let config = PurgedCvConfig::default();
        assert_eq!(config.purge_days, 16); // Y30 durability overlap
        assert_eq!(config.k_folds, 5);
    }
}
