//! Growth Evidence — the unified, immutable evidence record that all
//! learning subsystems consume.
//!
//! The brain has multiple learning subsystems: the causal model, treatment
//! effects, reach conversion, calibration, strategy learning. Previously,
//! each subsystem had its own idea of what happened — the causal model
//! read from `viryaos_brain_evidence`, the reach model read from
//! `viryaos_reach_events`, and the experiment engine logged propensities
//! separately.
//!
//! This module defines `GrowthEvidence` — a single immutable record that
//! captures the full evidence tuple for one dispatch: action, reach,
//! exposure, treatment assignment, propensity, outcome, prediction, and
//! context. All learning subsystems consume the same evidence, which
//! stops the brain from turning into 15 sophisticated subsystems with
//! slightly different ideas of what happened.
//!
//! # Lifecycle
//!
//! 1. **At dispatch time**: the brain records a `GrowthEvidence` row with
//!    the prediction, context, treatment assignment, and propensity. The
//!    outcome fields are `None`.
//! 2. **At measurement time**: the measurement system updates the evidence
//!    row with the observed outcome (fans, signal installs, durable fans).
//! 3. **At conversion time**: if a fan conversion is attributed, the
//!    `converted` and `converted_fan_id` fields are set.
//! 4. **At learning time**: the brain loads all resolved evidence rows and
//!    updates its posteriors from them.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::causal_model::DispatchContext;
use crate::experiment::TreatmentAssignment;
use crate::reach::ReachChannel;

/// A single immutable growth evidence record — the unified evidence that
/// all learning subsystems consume.
///
/// Each row captures the full evidence tuple for one dispatch: what the
/// brain predicted, what treatment was assigned, what reach was achieved,
/// and what outcome was observed. This is the single source of truth for
/// learning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrowthEvidence {
    /// The workspace this evidence belongs to.
    pub workspace_id: uuid::Uuid,
    /// The stable opportunity ID (template:target:action:context_hash).
    pub opportunity_id: Option<String>,
    /// The autopilot action that triggered this evidence.
    pub action_id: uuid::Uuid,
    /// When the evidence was recorded (dispatch time).
    pub timestamp: OffsetDateTime,

    // ── Reach ──
    /// The audience being targeted (subreddit name, platform, etc.).
    pub audience: Option<String>,
    /// The specific recipient identifier (fan ID, subreddit name, etc.).
    pub recipient_id: String,
    /// The channel used to reach the recipient.
    pub channel: ReachChannel,
    /// Estimated reach (1 for individuals, subscriber count for broadcasts).
    pub estimated_reach: u32,
    /// Actual observed reach (if measurable — e.g. post views, email opens).
    pub actual_reach: Option<u32>,

    // ── Treatment ──
    /// Whether this dispatch was treatment or control.
    pub treatment: TreatmentAssignment,
    /// The propensity (probability of treatment assignment) for IPW.
    pub propensity: f64,

    // ── Outcome ──
    /// Raw observed fan count in the measurement window.
    pub observed_fans: Option<f64>,
    /// Counterfactual-adjusted incremental fans (observed - baseline).
    pub observed_incremental_fans: Option<f64>,
    /// Durable fans still active 30 days after the measurement window.
    pub durable_fans_30d: Option<f64>,
    /// Whether this dispatch resulted in a fan conversion.
    pub converted: bool,
    /// The fan ID if a conversion was attributed.
    pub converted_fan_id: Option<uuid::Uuid>,

    // ── Prediction (what the brain expected) ──
    /// Predicted fan count before the dispatch.
    pub predicted_fans: f64,
    /// Predicted Signal installs before the dispatch.
    pub predicted_signal_installs: f64,
    /// The context features that informed the prediction.
    pub context: DispatchContext,
}

impl Default for GrowthEvidence {
    fn default() -> Self {
        Self {
            workspace_id: uuid::Uuid::nil(),
            opportunity_id: None,
            action_id: uuid::Uuid::nil(),
            timestamp: OffsetDateTime::now_utc(),
            audience: None,
            recipient_id: String::new(),
            channel: ReachChannel::default(),
            estimated_reach: 1,
            actual_reach: None,
            treatment: TreatmentAssignment::Treatment,
            propensity: 1.0,
            observed_fans: None,
            observed_incremental_fans: None,
            durable_fans_30d: None,
            converted: false,
            converted_fan_id: None,
            predicted_fans: 0.0,
            predicted_signal_installs: 0.0,
            context: DispatchContext::default(),
        }
    }
}

impl GrowthEvidence {
    /// Creates a new evidence record at dispatch time. Outcome fields are
    /// `None` — they are filled in when measurements arrive.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn at_dispatch(
        workspace_id: uuid::Uuid,
        action_id: uuid::Uuid,
        opportunity_id: Option<String>,
        recipient_id: String,
        channel: ReachChannel,
        estimated_reach: u32,
        treatment: TreatmentAssignment,
        propensity: f64,
        predicted_fans: f64,
        predicted_signal_installs: f64,
        context: DispatchContext,
    ) -> Self {
        Self {
            workspace_id,
            opportunity_id,
            action_id,
            timestamp: OffsetDateTime::now_utc(),
            audience: context.subreddit_type.clone(),
            recipient_id,
            channel,
            estimated_reach,
            actual_reach: None,
            treatment,
            propensity,
            observed_fans: None,
            observed_incremental_fans: None,
            durable_fans_30d: None,
            converted: false,
            converted_fan_id: None,
            predicted_fans,
            predicted_signal_installs,
            context,
        }
    }

    /// Returns true if this evidence has a resolved outcome (any observed
    /// value is present).
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        self.observed_fans.is_some()
            || self.observed_incremental_fans.is_some()
            || self.durable_fans_30d.is_some()
            || self.converted
    }

    /// Returns the best available outcome for learning. The brain prefers
    /// durable fans (Y30) over incremental fans over raw fan count.
    #[must_use]
    pub fn best_outcome(&self) -> Option<f64> {
        self.durable_fans_30d
            .or(self.observed_incremental_fans)
            .or(self.observed_fans)
    }

    /// Returns the prediction error (observed - predicted) for the best
    /// available outcome. This is the dopamine signal for learning.
    #[must_use]
    pub fn prediction_error(&self) -> Option<f64> {
        self.best_outcome()
            .map(|observed| observed - self.predicted_fans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reach::ReachChannel;

    #[test]
    fn evidence_at_dispatch_has_no_outcome() {
        let evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            Some("test:target:scan:ctx".to_owned()),
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
        );
        assert!(!evidence.is_resolved());
        assert_eq!(evidence.best_outcome(), None);
        assert_eq!(evidence.prediction_error(), None);
    }

    #[test]
    fn evidence_prefers_durable_fans() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            None,
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
        );
        evidence.observed_fans = Some(10.0);
        evidence.observed_incremental_fans = Some(5.0);
        evidence.durable_fans_30d = Some(3.0);
        // Should prefer durable (3.0) over incremental (5.0) over raw (10.0).
        assert_eq!(evidence.best_outcome(), Some(3.0));
    }

    #[test]
    fn evidence_prefers_incremental_over_raw() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            None,
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
        );
        evidence.observed_fans = Some(10.0);
        evidence.observed_incremental_fans = Some(5.0);
        assert_eq!(evidence.best_outcome(), Some(5.0));
    }

    #[test]
    fn evidence_prediction_error() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            None,
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
        );
        evidence.observed_fans = Some(8.0);
        // prediction_error = 8.0 - 2.0 = 6.0
        assert!((evidence.prediction_error().unwrap() - 6.0).abs() < 0.001);
    }

    #[test]
    fn evidence_serializes_and_deserializes() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            Some("test:target:scan:ctx".to_owned()),
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
        );
        evidence.observed_fans = Some(10.0);
        let json = serde_json::to_string(&evidence).expect("should serialize");
        let deserialized: GrowthEvidence = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(deserialized.action_id, evidence.action_id);
        assert_eq!(deserialized.observed_fans, Some(10.0));
        assert_eq!(deserialized.estimated_reach, 500);
    }

    #[test]
    fn evidence_converted_marks_resolved() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            None,
            "fan_1".to_owned(),
            ReachChannel::Email,
            1,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
        );
        evidence.converted = true;
        assert!(evidence.is_resolved());
    }
}
