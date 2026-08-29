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

    /// Returns the Y14 (14-day incremental) outcome for learning. Falls
    /// back to raw observed fans when incremental is not available.
    ///
    /// This is the early leading signal — the brain can learn from it
    /// sooner than Y30, but it's less reliable as a North Star target.
    #[must_use]
    pub fn y14_outcome(&self) -> Option<f64> {
        self.observed_incremental_fans.or(self.observed_fans)
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

/// A trait for learning subsystems that consume growth evidence.
///
/// The brain has multiple learning subsystems (causal model, reach model,
/// funnel model, strategy learner, etc.). Each subsystem that wants to
/// learn from evidence implements this trait. The brain's evidence
/// replay loop calls `consume_evidence` on each registered subsystem.
///
/// This trait ensures all subsystems receive the same evidence and
/// prevents the brain from turning into disconnected silos with different
/// ideas of what happened.
///
/// # Single-counting guarantee
///
/// Each evidence row is passed to each consumer exactly once per replay.
/// The consumer is responsible for not double-counting (e.g. by checking
/// whether it has already seen this evidence row).
#[allow(dead_code)] // TODO: wire into production path (next sprint)
pub trait EvidenceConsumer {
    /// Consumes a batch of growth evidence and updates the subsystem's
    /// internal state.
    ///
    /// Called once per autopilot cycle with all new evidence rows since
    /// the last cycle. The consumer should process each row exactly once.
    fn consume_evidence(&mut self, evidence: &[GrowthEvidence]);
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
        assert_eq!(evidence.y14_outcome(), None);
        assert_eq!(evidence.y30_outcome(), None);
        assert_eq!(evidence.y14_prediction_error(), None);
    }

    #[test]
    fn evidence_y14_and_y30_are_separate_targets() {
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
        // Y14 uses incremental (5.0), Y30 uses durable (3.0) — separate targets.
        assert_eq!(evidence.y14_outcome(), Some(5.0));
        assert_eq!(evidence.y30_outcome(), Some(3.0));
    }

    #[test]
    fn evidence_y14_falls_back_to_raw() {
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
        // No incremental → Y14 falls back to raw (10.0).
        assert_eq!(evidence.y14_outcome(), Some(10.0));
        // Y30 is still None.
        assert_eq!(evidence.y30_outcome(), None);
    }

    #[test]
    fn evidence_prediction_errors_are_separate() {
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
