//! Physical-release fulfillment invariants for the Latarnik/Beacon bounded context.
//!
//! HTTP and PostgreSQL adapters may represent these states as strings, but the
//! transition policy belongs here so an API handler cannot accidentally invent
//! a new lifecycle rule.

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconReleaseCampaignState {
    Draft,
    Open,
    Closed,
    Cancelled,
}

impl TryFrom<&str> for BeaconReleaseCampaignState {
    type Error = BeaconReleaseStateError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "draft" => Ok(Self::Draft),
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(BeaconReleaseStateError),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconReleaseRecipientState {
    Eligible,
    Notified,
    Confirmed,
    Prepared,
    Sent,
    Delivered,
    Declined,
    Expired,
    Cancelled,
}

impl TryFrom<&str> for BeaconReleaseRecipientState {
    type Error = BeaconReleaseStateError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "eligible" => Ok(Self::Eligible),
            "notified" => Ok(Self::Notified),
            "confirmed" => Ok(Self::Confirmed),
            "prepared" => Ok(Self::Prepared),
            "sent" => Ok(Self::Sent),
            "delivered" => Ok(Self::Delivered),
            "declined" => Ok(Self::Declined),
            "expired" => Ok(Self::Expired),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(BeaconReleaseStateError),
        }
    }
}

impl BeaconReleaseRecipientState {
    /// Returns whether an operator may perform this transition while the
    /// campaign is in the supplied lifecycle state.
    #[must_use]
    pub const fn can_transition_to(self, next: Self, campaign: BeaconReleaseCampaignState) -> bool {
        use BeaconReleaseCampaignState::{Closed, Open};
        use BeaconReleaseRecipientState::{
            Cancelled, Confirmed, Delivered, Eligible, Notified, Prepared, Sent,
        };

        match campaign {
            Open => matches!(
                (self, next),
                (Confirmed, Prepared)
                    | (Confirmed, Sent)
                    | (Prepared, Sent)
                    | (Sent, Delivered)
                    | (Eligible, Cancelled)
                    | (Notified, Cancelled)
                    | (Confirmed, Cancelled)
                    | (Prepared, Cancelled)
            ),
            // Closing the claim window must never strand parcels already handed
            // to the carrier. Delivery acknowledgement remains legal afterwards.
            Closed => matches!((self, next), (Sent, Delivered)),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("unknown Beacon physical-release lifecycle state")]
pub struct BeaconReleaseStateError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_campaign_still_accepts_sent_to_delivered() {
        assert!(BeaconReleaseRecipientState::Sent.can_transition_to(
            BeaconReleaseRecipientState::Delivered,
            BeaconReleaseCampaignState::Closed,
        ));
        assert!(!BeaconReleaseRecipientState::Prepared.can_transition_to(
            BeaconReleaseRecipientState::Sent,
            BeaconReleaseCampaignState::Closed,
        ));
    }

    #[test]
    fn open_campaign_preserves_fulfillment_and_cancellation_graph() {
        let open = BeaconReleaseCampaignState::Open;
        assert!(
            BeaconReleaseRecipientState::Confirmed
                .can_transition_to(BeaconReleaseRecipientState::Prepared, open)
        );
        assert!(
            BeaconReleaseRecipientState::Prepared
                .can_transition_to(BeaconReleaseRecipientState::Sent, open)
        );
        assert!(
            BeaconReleaseRecipientState::Sent
                .can_transition_to(BeaconReleaseRecipientState::Delivered, open)
        );
        assert!(
            !BeaconReleaseRecipientState::Delivered
                .can_transition_to(BeaconReleaseRecipientState::Sent, open)
        );
    }
}
