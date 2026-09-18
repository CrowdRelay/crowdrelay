//! Shared autonomy primitives used by ViryaOS bounded contexts.
//!
//! This module is deliberately tiny. It contains only stable domain vocabulary
//! shared by bounded contexts; orchestration, persistence and transport stay in
//! the application and infrastructure layers.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A bounded confidence value expressed in basis points (`0..=10_000`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Confidence(u16);

impl Confidence {
    pub const MIN: Self = Self(0);
    pub const MAX: Self = Self(10_000);

    /// Creates a validated confidence value.
    pub const fn from_basis_points(value: u16) -> Result<Self, ConfidenceError> {
        if value <= 10_000 {
            Ok(Self(value))
        } else {
            Err(ConfidenceError::OutOfRange)
        }
    }

    /// Creates a confidence value while saturating values above 100%.
    #[must_use]
    pub const fn saturating_from_basis_points(value: u16) -> Self {
        Self(if value > 10_000 { 10_000 } else { value })
    }

    /// Returns the confidence as basis points.
    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

/// Error returned when a confidence value is outside `0..=10_000`.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ConfidenceError {
    #[error("confidence must be between 0 and 10000 basis points")]
    OutOfRange,
}

/// Maximum authority granted to a bounded context.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    /// Measure what would have happened; emit no operator task or side effect.
    Observe,
    /// Surface a recommendation but never enqueue an executable action.
    Recommend,
    /// Prepare an action that must be explicitly approved by an operator.
    RequireApproval,
    /// Execute actions that satisfy deterministic domain and policy limits.
    BoundedAuto,
}

impl AutonomyLevel {
    /// Returns true when the level is permitted to enqueue an executable action.
    #[must_use]
    pub const fn may_enqueue(self) -> bool {
        matches!(self, Self::RequireApproval | Self::BoundedAuto)
    }

    /// Returns true when the level may execute without human approval.
    #[must_use]
    pub const fn may_auto_execute(self) -> bool {
        matches!(self, Self::BoundedAuto)
    }
}

/// Result of the shared authority gate after a bounded context has made a
/// deterministic business decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDisposition {
    ObserveOnly,
    RecommendOnly,
    RequireApproval,
    AutoExecute,
    Deny,
}

/// Applies authority and confidence gates without knowing anything about the
/// concrete business action. Financial and domain-specific constraints remain
/// owned by the bounded context that produced the decision.
#[must_use]
pub const fn disposition(
    level: AutonomyLevel,
    confidence: Confidence,
    minimum_confidence: Confidence,
) -> PolicyDisposition {
    if confidence.0 < minimum_confidence.0 {
        return PolicyDisposition::Deny;
    }

    match level {
        AutonomyLevel::Observe => PolicyDisposition::ObserveOnly,
        AutonomyLevel::Recommend => PolicyDisposition::RecommendOnly,
        AutonomyLevel::RequireApproval => PolicyDisposition::RequireApproval,
        AutonomyLevel::BoundedAuto => PolicyDisposition::AutoExecute,
    }
}

/// How many observations stand behind the confidence a context reported.
///
/// Separate from [`Confidence`] because they answer different questions and
/// only one of them is currently asked. Confidence is what an estimator says
/// about itself; this is how much evidence it saw. A Beta posterior updated
/// four times reports a confidence, and that confidence is not small — it is
/// simply unearned, and nothing downstream can tell the difference by looking
/// at the number.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EvidenceCount(pub i64);

impl EvidenceCount {
    /// No observations at all — the state every estimator starts in.
    pub const NONE: Self = Self(0);

    /// Whether this many observations clears `floor`.
    #[must_use]
    pub const fn clears(self, floor: i64) -> bool {
        self.0 >= floor
    }
}

/// [`disposition`] with the evidence floor applied.
///
/// # Why a second gate
///
/// `disposition` asks how confident a context is and never asks what that
/// confidence was computed from. Those come apart exactly where it costs the
/// most: an estimator with four observations can report high confidence, clear
/// `minimum_confidence`, and be handed `AutoExecute` over an action the tenant
/// cannot take back. `action_class` already records that a venue, curator or
/// press contact gets one first approach — spending it on an unevaluated
/// posterior is not a small error, and it is not recoverable by learning later.
///
/// # Why this caps rather than denies
///
/// Below the floor the result is [`PolicyDisposition::RequireApproval`], never
/// `ObserveOnly` and never `Deny`. Denying would be self-defeating: acting is
/// how the observations that clear the floor get made, so a gate that blocks
/// action below the floor guarantees the floor is never reached. What must not
/// happen is *unattended* action on an untested estimator, so the action still
/// goes out — with a person on it.
///
/// `Deny` from the confidence gate is preserved. A context that failed its own
/// confidence bar is refused, and having little evidence does not soften that.
///
/// The floor is the caller's, because the caller knows what it is counting.
/// `measurement::RATE_FLOOR` is the floor for rates and the right default for
/// anything counted per-observation.
#[must_use]
pub const fn disposition_with_evidence(
    level: AutonomyLevel,
    confidence: Confidence,
    minimum_confidence: Confidence,
    observations: EvidenceCount,
    floor: i64,
) -> PolicyDisposition {
    let granted = disposition(level, confidence, minimum_confidence);
    if observations.clears(floor) {
        return granted;
    }
    match granted {
        // The one downgrade: unattended execution becomes attended.
        PolicyDisposition::AutoExecute => PolicyDisposition::RequireApproval,
        // Already at or below approval, or refused outright. A thin posterior
        // is no reason to widen authority, and no reason to narrow one that is
        // already narrow.
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_rejects_values_above_one_hundred_percent() {
        assert_eq!(
            Confidence::from_basis_points(10_001),
            Err(ConfidenceError::OutOfRange)
        );
    }

    #[test]
    fn confidence_saturation_is_explicit_and_bounded() {
        assert_eq!(
            Confidence::saturating_from_basis_points(u16::MAX),
            Confidence::MAX
        );
    }

    #[test]
    fn authority_never_escalates_below_confidence_threshold() {
        let confidence = Confidence::saturating_from_basis_points(7_999);
        let minimum = Confidence::saturating_from_basis_points(8_000);

        assert_eq!(
            disposition(AutonomyLevel::BoundedAuto, confidence, minimum),
            PolicyDisposition::Deny
        );
    }

    #[test]
    fn bounded_auto_is_the_only_level_that_can_auto_execute() {
        let minimum = Confidence::saturating_from_basis_points(8_000);

        assert_eq!(
            disposition(AutonomyLevel::BoundedAuto, Confidence::MAX, minimum),
            PolicyDisposition::AutoExecute
        );
        assert_eq!(
            disposition(AutonomyLevel::RequireApproval, Confidence::MAX, minimum),
            PolicyDisposition::RequireApproval
        );
        assert!(!AutonomyLevel::RequireApproval.may_auto_execute());
        assert!(AutonomyLevel::RequireApproval.may_enqueue());
    }

    /// The case the confidence gate alone cannot see: a maximally confident
    /// estimator that has been tested four times.
    #[test]
    fn a_confident_estimator_with_thin_evidence_cannot_execute_unattended() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        let floor = crate::measurement::RATE_FLOOR;

        assert_eq!(
            disposition(AutonomyLevel::BoundedAuto, Confidence::MAX, minimum),
            PolicyDisposition::AutoExecute,
            "the confidence gate alone grants unattended execution"
        );
        assert_eq!(
            disposition_with_evidence(
                AutonomyLevel::BoundedAuto,
                Confidence::MAX,
                minimum,
                EvidenceCount(4),
                floor,
            ),
            PolicyDisposition::RequireApproval,
            "four observations must not license an unattended irreversible action"
        );
    }

    #[test]
    fn clearing_the_floor_changes_nothing() {
        let minimum = Confidence::saturating_from_basis_points(8_000);
        let floor = crate::measurement::RATE_FLOOR;

        for level in [
            AutonomyLevel::Observe,
            AutonomyLevel::Recommend,
            AutonomyLevel::RequireApproval,
            AutonomyLevel::BoundedAuto,
        ] {
            assert_eq!(
                disposition_with_evidence(
                    level,
                    Confidence::MAX,
                    minimum,
                    EvidenceCount(floor),
                    floor,
                ),
                disposition(level, Confidence::MAX, minimum),
                "at the floor the evidence gate is transparent"
            );
        }
    }

    /// Below the floor is unproven, not refused. Blocking action below the
    /// floor would stop the observations that clear it from ever being made.
    #[test]
    fn thin_evidence_never_narrows_authority_past_approval() {
        let minimum = Confidence::saturating_from_basis_points(8_000);

        for level in [
            AutonomyLevel::Observe,
            AutonomyLevel::Recommend,
            AutonomyLevel::RequireApproval,
        ] {
            let granted = disposition(level, Confidence::MAX, minimum);
            assert_eq!(
                disposition_with_evidence(
                    level,
                    Confidence::MAX,
                    minimum,
                    EvidenceCount::NONE,
                    crate::measurement::RATE_FLOOR,
                ),
                granted,
                "the evidence floor only ever downgrades AutoExecute"
            );
        }
    }

    /// A context that failed its own confidence bar stays refused. Thin
    /// evidence must not turn a denial into an approval queue item.
    #[test]
    fn thin_evidence_does_not_soften_a_denial() {
        let confidence = Confidence::saturating_from_basis_points(7_999);
        let minimum = Confidence::saturating_from_basis_points(8_000);

        assert_eq!(
            disposition_with_evidence(
                AutonomyLevel::BoundedAuto,
                confidence,
                minimum,
                EvidenceCount::NONE,
                crate::measurement::RATE_FLOOR,
            ),
            PolicyDisposition::Deny
        );
    }
}
