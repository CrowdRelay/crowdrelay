//! Strongly typed UUID identifiers.
//!
//! Each domain entity has a dedicated newtype wrapper around `Uuid` to
//! prevent accidental mixing of identifiers at compile time. All identifiers
//! use UUID v7 (time-ordered) by default, which improves index locality in
//! PostgreSQL while remaining globally unique.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! typed_uuid_id {
    ($($name:ident),+ $(,)?) => {
        $(
            #[doc = concat!("Strongly typed UUID identifier for `", stringify!($name), "`.")]
            #[derive(
                Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
            )]
            #[serde(transparent)]
            pub struct $name(Uuid);

            impl $name {
                /// Creates a new time-ordered UUID version 7 identifier.
                #[must_use]
                pub fn new() -> Self {
                    Self(Uuid::now_v7())
                }

                /// Wraps an existing UUID without changing its value.
                #[must_use]
                pub const fn from_uuid(value: Uuid) -> Self {
                    Self(value)
                }

                /// Borrows the underlying UUID.
                #[must_use]
                pub const fn as_uuid(&self) -> &Uuid {
                    &self.0
                }

                /// Consumes the typed identifier and returns the underlying UUID.
                #[must_use]
                pub const fn into_uuid(self) -> Uuid {
                    self.0
                }
            }

            impl Default for $name {
                fn default() -> Self {
                    Self::new()
                }
            }

            impl fmt::Display for $name {
                fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    self.0.fmt(formatter)
                }
            }

            impl FromStr for $name {
                type Err = uuid::Error;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    Uuid::parse_str(value).map(Self)
                }
            }

            impl From<Uuid> for $name {
                fn from(value: Uuid) -> Self {
                    Self(value)
                }
            }

            impl From<$name> for Uuid {
                fn from(value: $name) -> Self {
                    value.0
                }
            }
        )+
    };
}

typed_uuid_id!(
    WorkspaceId,
    CampaignId,
    SmartLinkId,
    FanId,
    CityId,
    VisitorId,
    ReferralAttributionId,
    RewardRuleId,
    RewardGrantId,
    RewardDrawId,
    MerchCouponId,
    EventId,
    AdmissionPoolId,
    AdmissionPassId,
    PassSessionId,
    WorkspaceMemberId,
    WorkspaceMemberSessionId,
    TicketTypeId,
    MerchVariantId,
    MerchProductId,
    AutopilotDecisionId,
    AutopilotActionId,
    AutopilotMeasurementId,
    PromotionCampaignId,
    MarketSignalId,
    BookingTargetId,
    OutreachTargetId,
    OutreachOpportunityId,
    ContentSourceId,
    ExperimentId,
    ExperimentVariantId,
    ReleasePlanId,
    TeamOpportunityId,
    BeaconId,
    TeamAssignmentId,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_uuid_v7_and_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let id = WorkspaceId::new();
        assert_eq!(id.as_uuid().get_version_num(), 7);

        let encoded = id.to_string();
        assert_eq!(encoded.parse::<WorkspaceId>()?, id);
        assert_eq!(WorkspaceId::from_uuid(id.into_uuid()), id);
        Ok(())
    }
}
