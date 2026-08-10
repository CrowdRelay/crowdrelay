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
}
