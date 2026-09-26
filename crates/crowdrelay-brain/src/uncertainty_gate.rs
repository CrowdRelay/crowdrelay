//! When the brain's uncertainty has earned a say in what it does.
//!
//! Every decision records the Y30 posterior it was valued on (mean and
//! standard deviation, `decision_value.epistemic.posterior`). Once the
//! 30-day outcome resolves, the pair is scored here: did the outcome land
//! inside the posterior's own 80% interval? Over many decisions an honest
//! posterior lands there about 80% of the time; an overconfident one far
//! less, an underconfident one far more.
//!
//! Uncertainty stays out of ranking until that record says it can be
//! believed — [`RESOLVED_TO_OPEN`] scored outcomes with coverage inside
//! [`COVERAGE_BAND`]. Penalising uncertain candidates before then is how a
//! young learner stops learning; penalising them on a standard deviation
//! nobody has checked is ranking on a number that may be fiction. Once open,
//! exploit candidates are valued at a lower quantile of their posterior
//! (see `DecisionValue::with_uncertainty_penalty`): a band would rather have
//! a fan it can count on than a speculative one of the same mean.
//! Exploration and learning candidates are never penalised — uncertainty is
//! the reason they exist.

use serde::{Deserialize, Serialize};

/// Scored outcomes the gate needs before uncertainty enters selection.
pub const RESOLVED_TO_OPEN: u32 = 200;
/// z for a central 80% interval of a normal posterior.
pub const Z_80: f64 = 1.281_551_565_545;
/// Coverage an honest 80% interval shows, with room for sampling noise at
/// 200 outcomes (±~6 points at one standard error is ~±2.8; this is wider
/// on purpose, the gate is about gross miscalibration).
pub const COVERAGE_BAND: (f64, f64) = (0.70, 0.90);
/// The quantile an open gate values exploit candidates at: the 25th
/// percentile of the posterior, `mean - 0.6745 σ`.
pub const LOWER_QUANTILE_Z: f64 = 0.674_489_750_196;

/// How often resolved outcomes fell inside the decision-time 80% interval.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IntervalCoverage {
    /// Outcomes scored against a stored posterior.
    pub n: u32,
    /// Of those, how many landed inside the 80% interval.
    pub within_80: u32,
    /// Σ z² — its mean is the squared factor by which the posteriors'
    /// spread was off (1.0 when honest).
    pub sum_z_sq: f64,
}

impl IntervalCoverage {
    /// Scores one resolved outcome against the posterior it was decided on.
    /// A non-positive or non-finite standard deviation is not a posterior and
    /// is not scored.
    pub fn record(&mut self, mean: f64, std: f64, observed: f64) {
        if !(std.is_finite() && std > 0.0 && mean.is_finite() && observed.is_finite()) {
            return;
        }
        let z = (observed - mean) / std;
        self.n = self.n.saturating_add(1);
        if z.abs() <= Z_80 {
            self.within_80 = self.within_80.saturating_add(1);
        }
        self.sum_z_sq += z * z;
    }

    /// Share of scored outcomes inside the 80% interval; `None` with none.
    #[must_use]
    pub fn coverage(&self) -> Option<f64> {
        (self.n > 0).then(|| f64::from(self.within_80) / f64::from(self.n))
    }
}

/// Whether uncertainty may enter selection this cycle, and why.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct UncertaintyGate {
    pub open: bool,
    pub scored: u32,
    pub coverage: Option<f64>,
    pub reason: &'static str,
}

impl UncertaintyGate {
    /// One line for the cycle's dispatch log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "uncertainty gate {}: {} ({} outcomes scored{})",
            if self.open { "open" } else { "shut" },
            self.reason,
            self.scored,
            self.coverage
                .map(|rate| format!(", {:.0}% inside the 80% interval", rate * 100.0))
                .unwrap_or_default(),
        )
    }
}

#[must_use]
pub fn uncertainty_gate(coverage: &IntervalCoverage) -> UncertaintyGate {
    let rate = coverage.coverage();
    let (open, reason) = if coverage.n < RESOLVED_TO_OPEN {
        (
            false,
            "fewer than 200 outcomes scored against their posterior",
        )
    } else {
        match rate {
            Some(rate) if rate < COVERAGE_BAND.0 => (
                false,
                "posteriors are overconfident: outcomes miss the 80% interval too often",
            ),
            Some(rate) if rate > COVERAGE_BAND.1 => (
                false,
                "posteriors are underconfident: intervals are wider than outcomes need",
            ),
            Some(_) => (true, "posteriors are calibrated over 200+ outcomes"),
            None => (false, "no outcome scored"),
        }
    };
    UncertaintyGate {
        open,
        scored: coverage.n,
        coverage: rate,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(n: u32, within: u32) -> IntervalCoverage {
        IntervalCoverage {
            n,
            within_80: within,
            sum_z_sq: f64::from(n),
        }
    }

    #[test]
    fn an_outcome_inside_the_interval_counts_as_covered() {
        let mut coverage = IntervalCoverage::default();
        coverage.record(10.0, 2.0, 12.0);
        coverage.record(10.0, 2.0, 20.0);
        coverage.record(10.0, 0.0, 10.0);
        assert_eq!(coverage.n, 2, "a zero std is not a posterior");
        assert_eq!(coverage.within_80, 1);
        assert_eq!(coverage.coverage(), Some(0.5));
    }

    #[test]
    fn the_gate_stays_shut_until_two_hundred_calibrated_outcomes() {
        assert!(!uncertainty_gate(&scored(150, 120)).open);
        assert!(uncertainty_gate(&scored(200, 160)).open);
    }

    #[test]
    fn a_miscalibrated_posterior_keeps_the_gate_shut() {
        let over = uncertainty_gate(&scored(250, 125));
        assert!(!over.open);
        assert!(over.reason.contains("overconfident"));
        let under = uncertainty_gate(&scored(250, 245));
        assert!(!under.open);
        assert!(under.reason.contains("underconfident"));
    }
}
