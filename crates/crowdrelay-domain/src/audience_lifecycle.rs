//! Audience lifecycle bounded context.
//!
//! The context decides *whether* a lifecycle touch is appropriate. It never
//! contains an email address and never sends a message; current consent is
//! re-checked again by the delivery boundary immediately before emission.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{FanId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FanLifecycleSnapshot {
    pub fan_id: FanId,
    pub active: bool,
    pub marketing_consent: bool,
    pub synesthesia_completed_at: Option<OffsetDateTime>,
    pub last_marketing_touch_at: Option<OffsetDateTime>,
    pub has_paid_ticket: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FanLifecyclePolicy {
    pub minimum_hours_after_synesthesia: u32,
    pub marketing_cooldown_hours: u32,
}

impl Default for FanLifecyclePolicy {
    fn default() -> Self {
        Self {
            minimum_hours_after_synesthesia: 48,
            marketing_cooldown_hours: 120,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleTemplate {
    SynesthesiaFollowUp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanLifecycleDecision {
    Hold(FanLifecycleHoldReason),
    RequestMessage {
        template: LifecycleTemplate,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanLifecycleHoldReason {
    InvalidSnapshot,
    Inactive,
    NoConsent,
    NoSynesthesiaCompletion,
    TooEarly,
    CooldownActive,
    AlreadyConverted,
}

#[must_use]
pub fn evaluate_fan_lifecycle(
    snapshot: FanLifecycleSnapshot,
    policy: FanLifecyclePolicy,
    now: OffsetDateTime,
) -> FanLifecycleDecision {
    if snapshot
        .synesthesia_completed_at
        .is_some_and(|completed_at| completed_at > now)
        || snapshot
            .last_marketing_touch_at
            .is_some_and(|last_touch| last_touch > now)
    {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::InvalidSnapshot);
    }
    if !snapshot.active {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::Inactive);
    }
    if !snapshot.marketing_consent {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent);
    }
    if snapshot.has_paid_ticket {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::AlreadyConverted);
    }
    let Some(completed_at) = snapshot.synesthesia_completed_at else {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoSynesthesiaCompletion);
    };
    if now - completed_at < Duration::hours(i64::from(policy.minimum_hours_after_synesthesia)) {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::TooEarly);
    }
    if snapshot.last_marketing_touch_at.is_some_and(|last_touch| {
        now - last_touch < Duration::hours(i64::from(policy.marketing_cooldown_hours))
    }) {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::CooldownActive);
    }

    FanLifecycleDecision::RequestMessage {
        template: LifecycleTemplate::SynesthesiaFollowUp,
        confidence: Confidence::saturating_from_basis_points(9_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn eligible_snapshot() -> FanLifecycleSnapshot {
        FanLifecycleSnapshot {
            fan_id: FanId::new(),
            active: true,
            marketing_consent: true,
            synesthesia_completed_at: Some(now() - Duration::days(4)),
            last_marketing_touch_at: None,
            has_paid_ticket: false,
        }
    }

    #[test]
    fn consent_is_a_hard_invariant_not_a_confidence_signal() {
        let mut snapshot = eligible_snapshot();
        snapshot.marketing_consent = false;
        assert_eq!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent)
        );
    }

    #[test]
    fn completed_unconverted_fan_is_eligible_after_delay() {
        assert!(matches!(
            evaluate_fan_lifecycle(eligible_snapshot(), FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage { .. }
        ));
    }

    #[test]
    fn conversion_stops_the_sales_lifecycle() {
        let mut snapshot = eligible_snapshot();
        snapshot.has_paid_ticket = true;
        assert_eq!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::AlreadyConverted)
        );
    }

    #[test]
    fn future_completion_timestamp_fails_closed() {
        let mut snapshot = eligible_snapshot();
        snapshot.synesthesia_completed_at = Some(now() + Duration::hours(1));
        assert_eq!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::InvalidSnapshot)
        );
    }
}
