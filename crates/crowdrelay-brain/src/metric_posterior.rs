//! Posteriors for the secondary metrics — everything `observed_metrics`
//! carries that is not fan growth or Signal installs.
//!
//! Ticket revenue, replies, clicks, attendance, engagement and the quality
//! checkpoints all land in the evidence row's metric map under their
//! `learnable_metric_key`. This module is where those observations become
//! beliefs: one posterior per metric key, hierarchical over the template and
//! target that produced it, so "revenue from ticket_price dispatches on this
//! venue" can diverge from "revenue from show_budget dispatches" instead of
//! pooling into one meaningless average.
//!
//! A metric posterior is a *level* estimate — the expected value of the
//! measurement, not a treatment effect. These metrics are proximal outcomes:
//! they feed economic value, standing and harm learning, and none of them is
//! assumed to be a substitute for the fan-growth estimands the typed columns
//! carry. The map never contains a fan-growth key — those kinds return `None`
//! from `learnable_metric_key` precisely so one observation cannot reach two
//! learners.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::bayesian::{HierarchicalPosterior, NormalPosterior};
use crate::evidence::EvidenceQuality;

/// The learned belief about one secondary metric.
///
/// `effects` holds the hierarchy — a global level plus per-template and
/// per-target levels that shrink toward it — and `effective_observations`
/// accumulates quality-weighted mass per template, the same confidence
/// currency the treatment-effect model spends: a randomized holdout counts
/// for ten observational rows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricPosterior {
    /// Hierarchical posterior for the metric's level.
    pub effects: HierarchicalPosterior,
    /// Quality-weighted observation mass per template. Defaulted on
    /// deserialize so a checkpoint written before the field existed reports
    /// zero confidence — the safe direction, since the caller then falls
    /// back to whatever it used before the metric was learned.
    #[serde(default)]
    pub effective_observations: HashMap<String, f64>,
}

impl Default for MetricPosterior {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricPosterior {
    /// A metric posterior starts skeptical — mean 0, prior variance — so the
    /// first observations have to earn the estimate rather than inherit a
    /// guess compiled in for a different tenant.
    ///
    /// The prior variance is the *unscaled* floor. `update` reseeds it from
    /// the first observation's magnitude before folding — see below.
    #[must_use]
    pub fn new() -> Self {
        Self {
            effects: HierarchicalPosterior::new(NormalPosterior::prior(0.0, crate::PRIOR_VARIANCE)),
            effective_observations: HashMap::new(),
        }
    }

    /// Folds one resolved measurement into the hierarchy.
    ///
    /// `observation_variance` is the caller's noise estimate, already scaled
    /// by evidence quality; `quality` separately moves the confidence the way
    /// it does for the treatment-effect model — shifting the mean further and
    /// being believed are two different things, and a caller that scaled its
    /// variance has not thereby said anything about identification.
    ///
    /// # Scale seeding
    ///
    /// `PRIOR_VARIANCE` is calibrated for fan counts — single digits. A
    /// revenue observation of four thousand minor units against that prior
    /// needs tens of thousands of measurements before its precision outweighs
    /// the prior's, which is not skepticism, it is a posterior that never
    /// learns. So the first update this metric ever receives reseeds the
    /// global prior's variance to the observation's own scale — the prior
    /// stays centered at zero (it still asserts nothing about direction) but
    /// stops pretending a revenue measurement lives in single digits. Child
    /// levels inherit the reseeded prior through the usual pre-update
    /// snapshot, so the seeding happens before any level sees the value.
    ///
    /// Signed update: revenue deltas, reply counts and engagement scores can
    /// all legitimately read below zero (a refund net, a suppression), and a
    /// metric that clamps at zero is one the harm side of the ledger can
    /// never reach.
    pub fn update(
        &mut self,
        template_id: Option<&str>,
        target_key: Option<&str>,
        observation: f64,
        observation_variance: f64,
        quality: EvidenceQuality,
    ) {
        // While the global level is still mostly prior (fewer than three
        // observations), an observation beyond the prior's plausible range is
        // evidence the *scale* was wrong, not evidence about the level — a
        // metric whose first reading was a zero-revenue gig must not be
        // pinned near zero forever when the next gig sells four thousand.
        // Reseed upward only: shrinking the prior on small observations
        // would let one quiet week blind the model to the next loud one.
        let plausible_span = 4.0 * self.effects.global.std();
        if self.effects.global.n < 3 && observation.abs() * 4.0 > plausible_span {
            self.effects.global.variance =
                (4.0 * observation.abs()).powi(2).max(crate::PRIOR_VARIANCE);
        }
        self.effects.update_signed_with_target(
            template_id,
            None,
            target_key,
            observation,
            observation_variance,
        );
        if let Some(template) = template_id {
            *self
                .effective_observations
                .entry(template.to_owned())
                .or_insert(0.0) += quality.weight();
        }
    }

    /// The quality-weighted confidence for a template's estimate, floored to
    /// whole observations — same reading rule as the treatment-effect
    /// posterior's.
    #[must_use]
    pub fn confidence(&self, template_id: &str) -> u32 {
        let effective = self
            .effective_observations
            .get(template_id)
            .copied()
            .unwrap_or(0.0);
        (effective.max(0.0) + 1e-9).floor() as u32
    }

    /// `(mean, std, confidence)` for a template and target — the target level
    /// shrinking toward the template, the template toward the global, the same
    /// partial pooling every other hierarchy in the model uses.
    #[must_use]
    pub fn predict_stats(&self, template_id: &str, target_key: Option<&str>) -> (f64, f64, u32) {
        let (mean, variance) = self
            .effects
            .predict_for_target(template_id, None, target_key);
        (mean, variance.max(0.0).sqrt(), self.confidence(template_id))
    }
}
