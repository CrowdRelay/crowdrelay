//! Label portfolio policy: how one tenant operates a roster and how its
//! audiences may amplify each other.
//!
//! A consent edge is the whole governance model of cross-artist promotion:
//! direction, purpose, scope, monthly cap, cooldown and lifecycle are data,
//! not conventions. The policy here keeps every decision deterministic and
//! auditable:
//!
//! - **Fans never leave home.** An edge lets the *beneficiary's* message go
//!   out through the *owner's* channels to the owner's own active fans. No
//!   fan rows move, no emails are copied into another workspace.
//! - **A consent is earned twice.** Proposed edges do nothing until approved;
//!   paused or revoked edges stop producing audience immediately.
//! - **Caps outvote ambition.** A monthly delivery cap and per-fan cooldown
//!   bound how loud one artist can be inside another artist's audience.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AmplificationPurpose {
    /// Ongoing mutual promotion between two roster artists.
    CrossPromote,
    /// One beneficiary release featured to the owner's audience.
    ReleaseFeature,
    /// Shared-billing push around a co-attended event or festival slot.
    EventCrossbill,
}

impl AmplificationPurpose {
    pub const ALL: [AmplificationPurpose; 3] = [
        AmplificationPurpose::CrossPromote,
        AmplificationPurpose::ReleaseFeature,
        AmplificationPurpose::EventCrossbill,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CrossPromote => "cross_promote",
            Self::ReleaseFeature => "release_feature",
            Self::EventCrossbill => "event_crossbill",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentStatus {
    Proposed,
    Active,
    Paused,
    Revoked,
}

impl ConsentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Revoked => "revoked",
        }
    }

    pub fn from_storage(value: &str) -> Option<Self> {
        match value {
            "proposed" => Some(Self::Proposed),
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// The operator actions on a consent edge and the states they may start from.
/// Revocation is terminal by design: a burned edge is re-proposed as a fresh
/// row with a new paper trail, never silently resurrected.
pub const ALLOWED_DECISIONS: &[(ConsentStatus, ConsentStatus)] = &[
    (ConsentStatus::Proposed, ConsentStatus::Active),
    (ConsentStatus::Proposed, ConsentStatus::Revoked),
    (ConsentStatus::Active, ConsentStatus::Paused),
    (ConsentStatus::Active, ConsentStatus::Revoked),
    (ConsentStatus::Paused, ConsentStatus::Active),
    (ConsentStatus::Paused, ConsentStatus::Revoked),
];

#[must_use]
pub fn can_decide(current: ConsentStatus, action_target: ConsentStatus) -> bool {
    ALLOWED_DECISIONS
        .iter()
        .any(|(from, to)| *from == current && *to == action_target)
}

/// Whether one more amplification delivery may leave through this edge.
///
/// `fan_eligible` carries everything about the individual fan the database
/// already knows better than this policy: active status, opt-in scope match,
/// unsubscribe state, per-fan cooldown. The policy adds the edge-level facts:
/// only an active consent produces audience, and the monthly cap binds.
#[must_use]
pub fn delivery_allowed(
    status: ConsentStatus,
    fan_eligible: bool,
    month_delivered: i64,
    max_campaigns_per_month: i32,
) -> bool {
    if status != ConsentStatus::Active || !fan_eligible {
        return false;
    }
    i64::from(max_campaigns_per_month.max(0)) > month_delivered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_round_trip_is_total() {
        for purpose in AmplificationPurpose::ALL {
            assert_eq!(
                AmplificationPurpose::from_storage(purpose.as_str()),
                Some(purpose)
            );
        }
        for status in [
            ConsentStatus::Proposed,
            ConsentStatus::Active,
            ConsentStatus::Paused,
            ConsentStatus::Revoked,
        ] {
            assert_eq!(ConsentStatus::from_storage(status.as_str()), Some(status));
        }
        assert_eq!(AmplificationPurpose::from_storage("spam"), None);
        assert_eq!(ConsentStatus::from_storage("draft"), None);
    }

    #[test]
    fn proposals_activate_but_revocation_is_terminal() {
        assert!(can_decide(ConsentStatus::Proposed, ConsentStatus::Active));
        assert!(can_decide(ConsentStatus::Active, ConsentStatus::Revoked));
        // Nothing comes back from revoked.
        for target in [
            ConsentStatus::Proposed,
            ConsentStatus::Active,
            ConsentStatus::Paused,
        ] {
            assert!(!can_decide(ConsentStatus::Revoked, target));
        }
    }

    #[test]
    fn pause_is_reversible_until_revoked() {
        assert!(can_decide(ConsentStatus::Active, ConsentStatus::Paused));
        assert!(can_decide(ConsentStatus::Paused, ConsentStatus::Active));
    }

    #[test]
    fn deliveries_need_active_consent_and_headroom() {
        let eligible = true;
        assert!(delivery_allowed(ConsentStatus::Active, eligible, 0, 2));
        // Cap reached for the calendar month.
        assert!(!delivery_allowed(ConsentStatus::Active, eligible, 2, 2));
        // Ineligible fan (unsubscribed, cooldown, wrong scope): never.
        assert!(!delivery_allowed(ConsentStatus::Active, false, 0, 2));
        // Any non-active state produces no audience at all.
        for status in [
            ConsentStatus::Proposed,
            ConsentStatus::Paused,
            ConsentStatus::Revoked,
        ] {
            assert!(!delivery_allowed(status, eligible, 0, 12));
        }
    }
}
