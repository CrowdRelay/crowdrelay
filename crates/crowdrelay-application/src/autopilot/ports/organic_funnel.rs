//! The organic funnel's control vocabulary — the mature verified-organic
//! funnel's current limiting stage, read per cycle so the brain does not
//! add signups ahead of a downstream leak. Split out of `ports.rs` under the
//! source-size ratchet; re-exported as `ports::organic_funnel::*` like
//! `booking_discovery`.

use crowdrelay_domain::FanId;
use time::OffsetDateTime;
use uuid::Uuid;

pub const CONFIRMATION_RECOVERY_TEMPLATE: &str = "crowdrelay.fan.confirmation_recovery.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrganicFunnelDirective {
    ExpandReach,
    RepairConversion,
    RepairConfirmation,
    ActivateFans,
    RetainFans,
}

impl OrganicFunnelDirective {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExpandReach => "expand_reach",
            Self::RepairConversion => "repair_conversion",
            Self::RepairConfirmation => "repair_confirmation",
            Self::ActivateFans => "activate_fans",
            Self::RetainFans => "retain_fans",
        }
    }

    #[must_use]
    pub const fn permits_join_ask(self) -> bool {
        matches!(self, Self::ExpandReach | Self::RepairConversion)
    }

    /// True once verified traffic already exists and the limiting loss is
    /// downstream of acquisition. New public/community acquisition work then
    /// waits; owned-fan recovery may continue.
    #[must_use]
    pub const fn holds_new_audience_expansion(self) -> bool {
        matches!(
            self,
            Self::RepairConfirmation | Self::ActivateFans | Self::RetainFans
        )
    }

    /// A downstream leak makes FanLifecycle the first context that may spend
    /// the cycle's owned-audience envelope.
    #[must_use]
    pub const fn prioritizes_fan_recovery(self) -> bool {
        self.holds_new_audience_expansion()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct OrganicFunnelControl {
    pub directive: OrganicFunnelDirective,
    pub mature_links: u32,
    pub unique_visitors: u32,
    pub signups: u32,
    pub confirmed: u32,
    pub activation_mature: u32,
    pub activated_mature: u32,
    pub retention_mature: u32,
    pub retained: u32,
}

/// One pending fan whose original double-opt-in email did not reach a live
/// delivery path. This is not "they did not click": the snapshot exists only
/// after the latest confirmation event has no delivered/in-flight webhook
/// delivery and has terminal dead/cancelled evidence.
///
/// The source action/link fields keep the recovery tied to an attributable
/// CrowdRelay acquisition instead of turning arbitrary pending imports into
/// autonomous contact.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ConfirmationRecoverySnapshot {
    pub fan_id: FanId,
    pub source_action_id: Uuid,
    pub source_target: String,
    #[serde(with = "time::serde::rfc3339")]
    pub acquired_at: OffsetDateTime,
    pub failed_outbox_event_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub failed_event_created_at: OffsetDateTime,
    pub failure_kind: String,
}
