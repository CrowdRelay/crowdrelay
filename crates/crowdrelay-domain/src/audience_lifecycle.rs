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
    pub created_at: OffsetDateTime,
    pub synesthesia_completed_at: Option<OffsetDateTime>,
    pub last_marketing_touch_at: Option<OffsetDateTime>,
    pub has_paid_ticket: bool,
    pub last_paid_ticket_at: Option<OffsetDateTime>,
    pub last_event_interest_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct FanLifecyclePolicy {
    pub welcome_after_hours: u32,
    pub minimum_hours_after_synesthesia: u32,
    pub marketing_cooldown_hours: u32,
    pub dormant_after_days: u32,
}

impl Default for FanLifecyclePolicy {
    fn default() -> Self {
        Self {
            welcome_after_hours: 24,
            minimum_hours_after_synesthesia: 48,
            marketing_cooldown_hours: 120,
            dormant_after_days: 60,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleTemplate {
    Welcome,
    SynesthesiaFollowUp,
    DormantReactivation,
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
    TooEarly,
    CooldownActive,
    AlreadyConverted,
    NoLifecycleOpportunity,
}

#[must_use]
pub fn evaluate_fan_lifecycle(
    snapshot: FanLifecycleSnapshot,
    policy: FanLifecyclePolicy,
    now: OffsetDateTime,
) -> FanLifecycleDecision {
    if snapshot.created_at > now
        || snapshot.synesthesia_completed_at.is_some_and(|at| at > now)
        || snapshot.last_marketing_touch_at.is_some_and(|at| at > now)
        || snapshot.last_paid_ticket_at.is_some_and(|at| at > now)
        || snapshot.last_event_interest_at.is_some_and(|at| at > now)
    {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::InvalidSnapshot);
    }
    if !snapshot.active {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::Inactive);
    }
    if !snapshot.marketing_consent {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent);
    }
    if snapshot.last_marketing_touch_at.is_some_and(|last_touch| {
        now - last_touch < Duration::hours(i64::from(policy.marketing_cooldown_hours))
    }) {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::CooldownActive);
    }

    if snapshot.last_marketing_touch_at.is_none()
        && now - snapshot.created_at >= Duration::hours(i64::from(policy.welcome_after_hours))
    {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::Welcome,
            confidence: Confidence::saturating_from_basis_points(9_700),
        };
    }

    if !snapshot.has_paid_ticket
        && let Some(completed_at) = snapshot.synesthesia_completed_at
        && now - completed_at >= Duration::hours(i64::from(policy.minimum_hours_after_synesthesia))
    {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::SynesthesiaFollowUp,
            confidence: Confidence::saturating_from_basis_points(9_000),
        };
    }

    let latest_activity = snapshot
        .last_paid_ticket_at
        .into_iter()
        .chain(snapshot.last_event_interest_at)
        .chain(snapshot.synesthesia_completed_at)
        .max()
        .unwrap_or(snapshot.created_at);
    if now - latest_activity >= Duration::days(i64::from(policy.dormant_after_days)) {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::DormantReactivation,
            confidence: Confidence::saturating_from_basis_points(8_600),
        };
    }

    if snapshot.has_paid_ticket {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::AlreadyConverted);
    }
    FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoLifecycleOpportunity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }
    fn eligible() -> FanLifecycleSnapshot {
        FanLifecycleSnapshot {
            fan_id: FanId::new(),
            active: true,
            marketing_consent: true,
            created_at: now() - Duration::days(10),
            synesthesia_completed_at: None,
            last_marketing_touch_at: None,
            has_paid_ticket: false,
            last_paid_ticket_at: None,
            last_event_interest_at: None,
        }
    }
    #[test]
    fn first_touch_is_a_welcome_without_requiring_synesthesia() {
        assert!(matches!(
            evaluate_fan_lifecycle(eligible(), FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::Welcome,
                ..
            }
        ));
    }
    #[test]
    fn consent_is_a_hard_gate() {
        let mut s = eligible();
        s.marketing_consent = false;
        assert_eq!(
            evaluate_fan_lifecycle(s, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent)
        );
    }
    #[test]
    fn dormant_fans_get_one_reactivation_after_cooldown() {
        let mut s = eligible();
        s.last_marketing_touch_at = Some(now() - Duration::days(90));
        s.created_at = now() - Duration::days(120);
        assert!(matches!(
            evaluate_fan_lifecycle(s, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::DormantReactivation,
                ..
            }
        ));
    }
}
