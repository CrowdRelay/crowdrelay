//! The organic funnel's control vocabulary — the mature verified-organic
//! funnel's current limiting stage, read per cycle so the brain does not
//! add signups ahead of a downstream leak. Split out of `ports.rs` under the
//! source-size ratchet; re-exported as `ports::organic_funnel::*` like
//! `booking_discovery`.

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
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
