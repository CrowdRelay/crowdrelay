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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconReleaseCampaignPhase {
    Draft,
    ClaimsOpen,
    Fulfillment,
    Completed,
    Cancelled,
}

impl BeaconReleaseCampaignPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::ClaimsOpen => "claims_open",
            Self::Fulfillment => "fulfillment",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BeaconReleaseProgress {
    pub confirmed: i64,
    pub prepared: i64,
    pub sent: i64,
}

impl BeaconReleaseProgress {
    #[must_use]
    pub const fn has_open_fulfillment(self) -> bool {
        self.confirmed > 0 || self.prepared > 0 || self.sent > 0
    }
}

impl BeaconReleaseCampaignState {
    #[must_use]
    pub fn phase(
        self,
        claim_deadline: time::OffsetDateTime,
        progress: BeaconReleaseProgress,
        now: time::OffsetDateTime,
    ) -> BeaconReleaseCampaignPhase {
        match self {
            Self::Draft => BeaconReleaseCampaignPhase::Draft,
            Self::Cancelled => BeaconReleaseCampaignPhase::Cancelled,
            Self::Open if now <= claim_deadline => BeaconReleaseCampaignPhase::ClaimsOpen,
            Self::Open => BeaconReleaseCampaignPhase::Fulfillment,
            Self::Closed if progress.has_open_fulfillment() => {
                BeaconReleaseCampaignPhase::Fulfillment
            }
            Self::Closed => BeaconReleaseCampaignPhase::Completed,
        }
    }
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

    #[test]
    fn campaign_phase_separates_claim_window_from_fulfillment() {
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let future = now + time::Duration::days(1);
        let past = now - time::Duration::days(1);
        assert_eq!(
            BeaconReleaseCampaignState::Open.phase(future, BeaconReleaseProgress::default(), now,),
            BeaconReleaseCampaignPhase::ClaimsOpen,
        );
        assert_eq!(
            BeaconReleaseCampaignState::Open.phase(past, BeaconReleaseProgress::default(), now,),
            BeaconReleaseCampaignPhase::Fulfillment,
        );
        assert_eq!(
            BeaconReleaseCampaignState::Closed.phase(
                past,
                BeaconReleaseProgress {
                    sent: 1,
                    ..BeaconReleaseProgress::default()
                },
                now,
            ),
            BeaconReleaseCampaignPhase::Fulfillment,
        );
        assert_eq!(
            BeaconReleaseCampaignState::Closed.phase(past, BeaconReleaseProgress::default(), now,),
            BeaconReleaseCampaignPhase::Completed,
        );
    }
}
