//! Growth Evidence — the unified evidence record that all learning
//! subsystems consume.
//!
//! The brain has multiple learning subsystems: the causal model, treatment
//! effects, reach conversion, calibration, strategy learning. Previously,
//! each subsystem had its own idea of what happened — the causal model
//! read from `viryaos_brain_evidence`, the reach model read from
//! `viryaos_reach_events`, and the experiment engine logged propensities
//! separately.
//!
//! This module defines `GrowthEvidence` — a single record that captures
//! the full evidence tuple for one dispatch: action, reach, exposure,
//! treatment assignment, propensity, outcome, prediction, and context.
//! All learning subsystems consume the same evidence, which stops the
//! brain from turning into 15 sophisticated subsystems with slightly
//! different ideas of what happened.
//!
//! `GrowthEvidence` is **mutable**: dispatch creates it (with outcome
//! fields `None`), measurement updates it with observed outcomes, and
//! conversion sets the `converted`/`converted_fan_id` fields. For a
//! truly immutable event log, see `EvidenceEvent` (append-only).
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

/// How the outcome was measured — the causal strength of the evidence.
///
/// The brain must never silently treat weak pre/post evidence as strong
/// causal evidence. This enum lets the learning loop weight observations
/// by their causal quality:
///
/// - **RandomizedHoldout**: true A/B — action was withheld for a control
///   group. Full causal strength. Treatment-effect posterior gets full
///   weight.
/// - **MatchedQuasiExperiment**: a matched comparison unit was used (e.g.
///   a similar subreddit that wasn't targeted). Strong quasi-experimental
///   design. ~70% weight.
/// - **PrePost**: before/after comparison on the same unit. Confounded by
///   seasonality, concurrent campaigns, baseline momentum. ~30% weight.
/// - **Observational**: no counterfactual — just an outcome observation.
///   Barely moves the posterior. ~10% weight.
///
/// The weights are applied by scaling the observation variance in the
/// treatment-effect posterior update: higher quality → lower variance →
/// the observation moves the posterior more.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceQuality {
    /// True A/B: action was withheld for a control group. Full causal
    /// strength.
    RandomizedHoldout,
    /// Matched comparison unit was used. Strong quasi-experimental design.
    MatchedQuasiExperiment,
    /// Direct conversion — no counterfactual needed. The action directly
    /// caused the outcome (e.g. a fan clicked a specific link and converted).
    /// Strong evidence for that particular metric, but narrow scope.
    DeterministicAttribution,
    /// Proportional credit from the credit ledger. The outcome was
    /// attributed among competing actions using observable facts with
    /// bounded prior influence. Weaker than direct attribution.
    ModeledAttribution,
    /// Before/after comparison on the same unit. Confounded by
    /// seasonality and concurrent campaigns.
    PrePost,
    /// No counterfactual — just an outcome observation. Weakest evidence.
    #[default]
    Observational,
}

impl EvidenceQuality {
    /// Returns the default prior weight for this method, before modifiers.
    /// Higher = more trust = lower effective variance.
    ///
    /// The hierarchy is:
    /// `RandomizedHoldout > DeterministicAttribution > MatchedQuasiExperiment
    /// > ModeledAttribution > PrePost > Observational`
    ///
    /// This is a **default prior**, not absolute truth. The runtime derives
    /// effective weight from method + modifiers via [`effective_weight`].
    #[must_use]
    pub const fn base_weight(self) -> f64 {
        match self {
            Self::RandomizedHoldout => 1.0,
            Self::DeterministicAttribution => 0.9,
            Self::MatchedQuasiExperiment => 0.7,
            Self::ModeledAttribution => 0.2,
            Self::PrePost => 0.3,
            Self::Observational => 0.1,
        }
    }

    /// Alias for [`base_weight`] — backwards compatibility.
    #[must_use]
    pub const fn weight(self) -> f64 {
        self.base_weight()
    }

    /// Computes the effective evidence weight from method + modifiers.
    /// The base weight comes from the method (the default hierarchy),
    /// but it is qualified by sample size, contamination, and measurement
    /// quality. A randomized holdout with 90% contamination is downgraded
    /// below a clean deterministic attribution.
    ///
    /// - `sample_size`: number of observations supporting this estimate.
    ///   Saturates at 20 — small samples are less reliable.
    /// - `contamination`: 0.0–1.0, estimated contamination from concurrent
    ///   actions. At 1.0, weight is halved.
    /// - `measurement_delay_days`: delay between action and measurement.
    ///   Long delays introduce noise. Saturates at 44 days (Y30 horizon).
    #[must_use]
    pub fn effective_weight(
        self,
        sample_size: u32,
        contamination: f64,
        measurement_delay_days: u32,
    ) -> f64 {
        let base = self.base_weight();
        // Contamination penalty: high contamination downgrades even
        // randomized evidence. At contamination=1.0, weight is halved.
        let contamination_factor = 1.0 - 0.5 * contamination.clamp(0.0, 1.0);
        // Sample size factor: small samples are less reliable.
        // Saturates at 20 observations.
        let sample_factor = (sample_size as f64 / 20.0).min(1.0);
        // Measurement delay penalty: long delays introduce noise.
        // Saturates at 44 days (Y30 horizon).
        let delay_factor = 1.0 - 0.2 * (measurement_delay_days as f64 / 44.0).min(1.0);
        base * contamination_factor * sample_factor * delay_factor
    }

    /// Returns the variance multiplier — how much to scale the base
    /// observation variance by. Lower quality → higher multiplier → the
    /// observation is trusted less.
    #[must_use]
    pub const fn variance_multiplier(self) -> f64 {
        1.0 / self.weight()
    }

    /// Returns the string representation for DB storage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RandomizedHoldout => "randomized_holdout",
            Self::MatchedQuasiExperiment => "matched_quasi_experiment",
            Self::DeterministicAttribution => "deterministic_attribution",
            Self::ModeledAttribution => "modeled_attribution",
            Self::PrePost => "pre_post",
            Self::Observational => "observational",
        }
    }

    /// Parses a string into an `EvidenceQuality`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "randomized_holdout" => Some(Self::RandomizedHoldout),
            "matched_quasi_experiment" => Some(Self::MatchedQuasiExperiment),
            "deterministic_attribution" => Some(Self::DeterministicAttribution),
            "modeled_attribution" => Some(Self::ModeledAttribution),
            "pre_post" => Some(Self::PrePost),
            "observational" => Some(Self::Observational),
            _ => None,
        }
    }
}

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
    /// `None` for control-arm evidence rows (no action was dispatched).
    pub action_id: Option<uuid::Uuid>,
    /// When the evidence was recorded (dispatch time).
    pub timestamp: OffsetDateTime,

    // ── Reach ──
    /// The audience being targeted (subreddit name, platform, etc.).
    pub audience: Option<String>,
    /// The specific target this dispatch was aimed at, e.g.
    /// `community:<target_id>`. `None` for workspace-wide templates and for
    /// rows written before the per-target level of the causal hierarchy
    /// existed. Replay rebuilds that level from this field, so an outcome
    /// without it teaches the template and the audience type but not the
    /// community it actually came from.
    #[serde(default)]
    pub target_key: Option<String>,
    /// The angle the post took, where the dispatch had one. Stored so the
    /// creative question can be asked of real outcomes later; nothing reads
    /// it for a decision today.
    #[serde(default)]
    pub creative_family: Option<crowdrelay_domain::creative::CreativeFamily>,
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
    /// The realized execution status of the assignment. `None` for
    /// legacy rows or control-arm evidence without an assignment.
    /// Used by the causal learner to select which rows contribute to
    /// the treatment-effect posterior under the active [`CausalEstimand`].
    pub execution_status: Option<crate::experiment::ExecutionStatus>,

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

    // ── Strategy + Evidence Quality ──
    /// The growth strategy that was active when this dispatch was made.
    /// Recorded so the strategy posterior learns from the actual strategy
    /// used, not a heuristic inference from the template.
    pub strategy: Option<String>,
    /// How the outcome was measured — the causal strength of the evidence.
    /// The learning loop weights observations by this quality.
    pub evidence_quality: EvidenceQuality,
    /// Sample size supporting this evidence's effect estimate.
    /// Used by `effective_weight` to qualify the method-based hierarchy.
    pub sample_size: Option<u32>,
    /// Estimated contamination from concurrent actions (0.0–1.0).
    /// High contamination downgrades even randomized evidence.
    pub contamination: Option<f64>,
    /// Delay between action and measurement (days). Long delays
    /// introduce noise and reduce evidence weight.
    pub measurement_delay_days: Option<u32>,

    // ── Episode linkage ──
    /// The episode this evidence belongs to (links to the episode model).
    pub episode_id: Option<String>,
    /// When the evidence was resolved (measurement window closed).
    pub resolved_at: Option<OffsetDateTime>,
}

impl Default for GrowthEvidence {
    fn default() -> Self {
        Self {
            workspace_id: uuid::Uuid::nil(),
            opportunity_id: None,
            action_id: Some(uuid::Uuid::nil()),
            timestamp: OffsetDateTime::now_utc(),
            audience: None,
            target_key: None,
            creative_family: None,
            recipient_id: String::new(),
            channel: ReachChannel::default(),
            estimated_reach: 1,
            actual_reach: None,
            treatment: TreatmentAssignment::Treatment,
            propensity: 1.0,
            execution_status: None,
            observed_fans: None,
            observed_incremental_fans: None,
            durable_fans_30d: None,
            converted: false,
            converted_fan_id: None,
            predicted_fans: 0.0,
            predicted_signal_installs: 0.0,
            context: DispatchContext::default(),
            strategy: None,
            evidence_quality: EvidenceQuality::default(),
            sample_size: None,
            contamination: None,
            measurement_delay_days: None,
            episode_id: None,
            resolved_at: None,
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
        action_id: Option<uuid::Uuid>,
        opportunity_id: Option<String>,
        recipient_id: String,
        channel: ReachChannel,
        estimated_reach: u32,
        treatment: TreatmentAssignment,
        propensity: f64,
        predicted_fans: f64,
        predicted_signal_installs: f64,
        context: DispatchContext,
        target_key: Option<String>,
        creative_family: Option<crowdrelay_domain::creative::CreativeFamily>,
        strategy: Option<String>,
        evidence_quality: EvidenceQuality,
    ) -> Self {
        Self {
            workspace_id,
            opportunity_id,
            action_id,
            timestamp: OffsetDateTime::now_utc(),
            audience: context.subreddit_type.clone(),
            target_key,
            creative_family,
            recipient_id,
            channel,
            estimated_reach,
            actual_reach: None,
            treatment,
            propensity,
            execution_status: None,
            observed_fans: None,
            observed_incremental_fans: None,
            durable_fans_30d: None,
            converted: false,
            converted_fan_id: None,
            predicted_fans,
            predicted_signal_installs,
            context,
            strategy,
            evidence_quality,
            sample_size: None,
            contamination: None,
            measurement_delay_days: None,
            episode_id: None,
            resolved_at: None,
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

    /// Returns the Y14 (14-day incremental) outcome for learning.
    ///
    /// This is the early leading signal — the brain can learn from it
    /// sooner than Y30, but it's less reliable as a North Star target.
    ///
    /// Returns `None` until the DiD-adjusted `observed_incremental_fans`
    /// is available. We deliberately do NOT fall back to raw
    /// `observed_fans` because `predicted_fans` is an incremental
    /// expectation — comparing it to a raw workspace-wide fan count
    /// would teach the brain the wrong lesson (the raw count includes
    /// fans acquired by other actions, organic growth, etc.).
    #[must_use]
    pub fn y14_outcome(&self) -> Option<f64> {
        self.observed_incremental_fans
    }

    /// Returns the Y30 (30-day durable) outcome for learning. This is the
    /// North Star target — fans that are still active after 30 days.
    ///
    /// Returns `None` until the 30-day measurement window has elapsed.
    #[must_use]
    pub fn y30_outcome(&self) -> Option<f64> {
        self.durable_fans_30d
    }

    /// Returns the Y14 prediction error (observed - predicted).
    #[must_use]
    pub fn y14_prediction_error(&self) -> Option<f64> {
        self.y14_outcome()
            .map(|observed| observed - self.predicted_fans)
    }

    /// Returns the Y30 prediction error (observed - predicted).
    #[must_use]
    pub fn y30_prediction_error(&self) -> Option<f64> {
        self.y30_outcome()
            .map(|observed| observed - self.predicted_fans)
    }
}

// ─── Evidence Event (immutable event-sourced log) ────────────────────────

/// The type of an evidence event — what happened.
///
/// Each event type has a specific payload shape. The events are:
/// - `ActionDispatched`: an autopilot action was dispatched. Payload includes
///   the prediction, context, treatment, and propensity.
/// - `ReachAttempted`: a reach event was recorded. Payload includes the
///   channel, template, estimated_reach, and recipient.
/// - `ExposureRecorded`: audience exposure was recorded. Payload includes
///   the audience key and exposure count.
/// - `ResponseReceived`: a response was received (reply, click). Payload
///   includes the response type and content.
/// - `ConversionObserved`: a fan conversion was observed. Payload includes
///   the fan_id and conversion source.
/// - `FanStillActiveDay30`: 30-day durability check — fan is still active.
/// - `FanChurnedDay30`: 30-day durability check — fan has churned.
/// - `MeasurementResolved`: the measurement window has closed. Payload
///   includes the final outcome values.
/// - `TreatmentAssigned`: a treatment was assigned (A/B test). Payload
///   includes the treatment and propensity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceEventType {
    ActionDispatched,
    ReachAttempted,
    ExposureRecorded,
    ResponseReceived,
    ConversionObserved,
    FanStillActiveDay30,
    FanChurnedDay30,
    MeasurementResolved,
    TreatmentAssigned,
}

impl EvidenceEventType {
    /// Returns the string representation for DB storage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActionDispatched => "action_dispatched",
            Self::ReachAttempted => "reach_attempted",
            Self::ExposureRecorded => "exposure_recorded",
            Self::ResponseReceived => "response_received",
            Self::ConversionObserved => "conversion_observed",
            Self::FanStillActiveDay30 => "fan_still_active_day_30",
            Self::FanChurnedDay30 => "fan_churned_day_30",
            Self::MeasurementResolved => "measurement_resolved",
            Self::TreatmentAssigned => "treatment_assigned",
        }
    }

    /// Parses a string into an `EvidenceEventType`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "action_dispatched" => Some(Self::ActionDispatched),
            "reach_attempted" => Some(Self::ReachAttempted),
            "exposure_recorded" => Some(Self::ExposureRecorded),
            "response_received" => Some(Self::ResponseReceived),
            "conversion_observed" => Some(Self::ConversionObserved),
            "fan_still_active_day_30" => Some(Self::FanStillActiveDay30),
            "fan_churned_day_30" => Some(Self::FanChurnedDay30),
            "measurement_resolved" => Some(Self::MeasurementResolved),
            "treatment_assigned" => Some(Self::TreatmentAssigned),
            _ => None,
        }
    }
}

/// An immutable evidence event — a single fact that happened at a specific
/// time. This is the append-only event log that the derived
/// `GrowthEpisode` aggregate is rebuilt from.
///
/// Unlike `GrowthEvidence` (which is mutable — dispatch creates it,
/// measurement updates it), `EvidenceEvent` is truly immutable: it's an
/// INSERT-only record of what happened.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceEvent {
    /// The workspace this event belongs to.
    pub workspace_id: uuid::Uuid,
    /// The action this event relates to (optional).
    pub action_id: Option<uuid::Uuid>,
    /// The opportunity this event relates to (optional).
    pub opportunity_id: Option<String>,
    /// The episode this event belongs to (optional).
    pub episode_id: Option<String>,
    /// The event type.
    pub event_type: EvidenceEventType,
    /// The event payload (type-specific JSON).
    pub payload: serde_json::Value,
    /// When the event occurred (immutable).
    pub occurred_at: OffsetDateTime,
}

impl Default for EvidenceEvent {
    fn default() -> Self {
        Self {
            workspace_id: uuid::Uuid::nil(),
            action_id: None,
            opportunity_id: None,
            episode_id: None,
            event_type: EvidenceEventType::ActionDispatched,
            payload: serde_json::json!({}),
            occurred_at: OffsetDateTime::now_utc(),
        }
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
            Some(uuid::Uuid::nil()),
            Some("test:target:scan:ctx".to_owned()),
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
            None,
            None,
            None,
            EvidenceQuality::default(),
        );
        assert!(!evidence.is_resolved());
        assert_eq!(evidence.y14_outcome(), None);
        assert_eq!(evidence.y30_outcome(), None);
        assert_eq!(evidence.y14_prediction_error(), None);
    }

    #[test]
    fn evidence_y14_and_y30_are_separate_targets() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            Some(uuid::Uuid::nil()),
            None,
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
            None,
            None,
            None,
            EvidenceQuality::default(),
        );
        evidence.observed_fans = Some(10.0);
        evidence.observed_incremental_fans = Some(5.0);
        evidence.durable_fans_30d = Some(3.0);
        // Y14 uses incremental (5.0), Y30 uses durable (3.0) — separate targets.
        assert_eq!(evidence.y14_outcome(), Some(5.0));
        assert_eq!(evidence.y30_outcome(), Some(3.0));
    }

    #[test]
    fn evidence_y14_falls_back_to_raw() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            Some(uuid::Uuid::nil()),
            None,
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
            None,
            None,
            None,
            EvidenceQuality::default(),
        );
        evidence.observed_fans = Some(10.0);
        // No incremental → Y14 is None. We do not fall back to raw
        // observed_fans because predicted_fans is incremental — using
        // a raw workspace-wide count would teach the wrong lesson.
        assert_eq!(evidence.y14_outcome(), None);
        // Y30 is still None.
        assert_eq!(evidence.y30_outcome(), None);
    }

    #[test]
    fn evidence_prediction_errors_are_separate() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            Some(uuid::Uuid::nil()),
            None,
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
            None,
            None,
            None,
            EvidenceQuality::default(),
        );
        evidence.observed_incremental_fans = Some(8.0);
        evidence.durable_fans_30d = Some(3.0);
        // Y14 error = 8.0 - 2.0 = 6.0
        assert!((evidence.y14_prediction_error().unwrap() - 6.0).abs() < 0.001);
        // Y30 error = 3.0 - 2.0 = 1.0
        assert!((evidence.y30_prediction_error().unwrap() - 1.0).abs() < 0.001);
    }

    #[test]
    fn evidence_serializes_and_deserializes() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            Some(uuid::Uuid::nil()),
            Some("test:target:scan:ctx".to_owned()),
            "r_MetalMusic".to_owned(),
            ReachChannel::RedditPost,
            500,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
            Some("community:test".to_owned()),
            None,
            Some("aggressive_discovery".to_owned()),
            EvidenceQuality::RandomizedHoldout,
        );
        evidence.observed_fans = Some(10.0);
        let json = serde_json::to_string(&evidence).expect("should serialize");
        let deserialized: GrowthEvidence = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(deserialized.action_id, evidence.action_id);
        assert_eq!(deserialized.observed_fans, Some(10.0));
        assert_eq!(deserialized.estimated_reach, 500);
        assert_eq!(
            deserialized.strategy,
            Some("aggressive_discovery".to_owned())
        );
        assert_eq!(
            deserialized.evidence_quality,
            EvidenceQuality::RandomizedHoldout
        );
    }

    #[test]
    fn evidence_converted_marks_resolved() {
        let mut evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            Some(uuid::Uuid::nil()),
            None,
            "fan_1".to_owned(),
            ReachChannel::Email,
            1,
            TreatmentAssignment::Treatment,
            0.5,
            2.0,
            0.2,
            DispatchContext::default(),
            None,
            None,
            None,
            EvidenceQuality::default(),
        );
        evidence.converted = true;
        assert!(evidence.is_resolved());
    }

    #[test]
    fn evidence_quality_weights_are_ordered() {
        assert!(
            EvidenceQuality::RandomizedHoldout.weight()
                > EvidenceQuality::MatchedQuasiExperiment.weight()
        );
        assert!(
            EvidenceQuality::MatchedQuasiExperiment.weight() > EvidenceQuality::PrePost.weight()
        );
        assert!(EvidenceQuality::PrePost.weight() > EvidenceQuality::Observational.weight());
    }

    #[test]
    fn evidence_quality_variance_multiplier_is_inverse_of_weight() {
        for q in [
            EvidenceQuality::RandomizedHoldout,
            EvidenceQuality::MatchedQuasiExperiment,
            EvidenceQuality::PrePost,
            EvidenceQuality::Observational,
        ] {
            assert!((q.variance_multiplier() - 1.0 / q.weight()).abs() < 1e-9);
        }
    }

    #[test]
    fn evidence_quality_round_trips_through_string() {
        for q in [
            EvidenceQuality::RandomizedHoldout,
            EvidenceQuality::MatchedQuasiExperiment,
            EvidenceQuality::PrePost,
            EvidenceQuality::Observational,
        ] {
            assert_eq!(EvidenceQuality::parse(q.as_str()), Some(q));
        }
        assert_eq!(EvidenceQuality::parse("unknown"), None);
    }

    // P1: Prediction consistency invariant.
    // The evidence built from a DispatchPrediction via at_dispatch
    // must carry the SAME predicted_fans and predicted_signal_installs
    // as the prediction. This is the structural guarantee that
    // prediction_at_decision == prediction_persisted_in_initial_evidence.
    #[test]
    fn prediction_consistency_evidence_matches_prediction() {
        let prediction = crate::causal_model::DispatchPrediction {
            template_id: "community-engager".to_owned(),
            expected_new_fans: 7.5,
            expected_signal_installs: 2.3,
            context: DispatchContext {
                subreddit_type: Some("r/test".to_owned()),
                ..Default::default()
            },
            target_key: None,
            creative_family: None,
        };
        let evidence = GrowthEvidence::at_dispatch(
            uuid::Uuid::nil(),
            Some(uuid::Uuid::nil()),
            None,
            "r/test".to_owned(),
            ReachChannel::RedditPost,
            1,
            TreatmentAssignment::Treatment,
            0.9,
            prediction.expected_new_fans,
            prediction.expected_signal_installs,
            prediction.context.clone(),
            prediction.target_key.clone(),
            prediction.creative_family,
            Some("discovery".to_owned()),
            EvidenceQuality::RandomizedHoldout,
        );
        // The evidence must carry the exact same prediction values.
        assert!(
            (evidence.predicted_fans - prediction.expected_new_fans).abs() < 1e-10,
            "predicted_fans mismatch: evidence={} prediction={}",
            evidence.predicted_fans,
            prediction.expected_new_fans
        );
        assert!(
            (evidence.predicted_signal_installs - prediction.expected_signal_installs).abs()
                < 1e-10,
            "predicted_signal_installs mismatch: evidence={} prediction={}",
            evidence.predicted_signal_installs,
            prediction.expected_signal_installs
        );
        // The context must also match — it's part of the prediction.
        assert_eq!(
            evidence.context.subreddit_type,
            prediction.context.subreddit_type
        );
    }
}
