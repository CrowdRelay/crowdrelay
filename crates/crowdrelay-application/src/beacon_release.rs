//! Physical-release use-case rules for the Latarnik/Beacon bounded context.
//!
//! The HTTP adapter owns authentication/DTO mapping; lifecycle authorization and
//! server-owned follow-up copy live here so they are reusable and directly testable.

use crowdrelay_domain::{BeaconReleaseCampaignState, BeaconReleaseRecipientState};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BeaconReleaseRecipientTransition {
    pub current: BeaconReleaseRecipientState,
    pub next: BeaconReleaseRecipientState,
    pub campaign: BeaconReleaseCampaignState,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BeaconReleaseTransitionError {
    #[error("requested Beacon release recipient state is invalid")]
    InvalidRequestedState,
    #[error("persisted Beacon release lifecycle state is invalid")]
    InvalidPersistedState,
    #[error("Beacon release recipient transition conflicts with campaign lifecycle")]
    Conflict,
}

pub fn validate_beacon_release_recipient_transition(
    current: &str,
    requested: &str,
    campaign: &str,
) -> Result<BeaconReleaseRecipientTransition, BeaconReleaseTransitionError> {
    let current = BeaconReleaseRecipientState::try_from(current)
        .map_err(|_| BeaconReleaseTransitionError::InvalidPersistedState)?;
    let next = BeaconReleaseRecipientState::try_from(requested)
        .map_err(|_| BeaconReleaseTransitionError::InvalidRequestedState)?;
    let campaign = BeaconReleaseCampaignState::try_from(campaign)
        .map_err(|_| BeaconReleaseTransitionError::InvalidPersistedState)?;
    if !current.can_transition_to(next, campaign) {
        return Err(BeaconReleaseTransitionError::Conflict);
    }
    Ok(BeaconReleaseRecipientTransition {
        current,
        next,
        campaign,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeaconReleaseActivationCopy {
    pub subject: String,
    pub text: String,
}

#[must_use]
pub fn beacon_release_activation_copy(
    locale: &str,
    display_name: &str,
    title: &str,
    member_url: &str,
) -> BeaconReleaseActivationCopy {
    if locale.starts_with("pl") {
        BeaconReleaseActivationCopy {
            subject: format!("Latarniku — jak siadło {title}?"),
            text: format!(
                "Hej {display_name}!\n\nMinęły dwa dni od dostarczenia {title}. Jak siadło wydanie? Jeśli masz ochotę pomóc przy tej premierze, w Press Roomie znajdziesz gotowe materiały do recenzji, radia/podcastu, zdjęć, wideo lub udostępnienia. Nic z tego nie jest obowiązkiem — dzięki, że jesteś częścią Latarnika.\n\n{member_url}\n\nVirya"
            ),
        }
    } else {
        BeaconReleaseActivationCopy {
            subject: format!("Beacon — how did {title} land?"),
            text: format!(
                "Hey {display_name}!\n\nIt has been two days since {title} was delivered. How did the release land? If you feel like helping with this release, the Press Room has ready material for reviews, radio/podcasts, photos, video or sharing. None of this is an obligation — thank you for being part of Beacon.\n\n{member_url}\n\nVirya"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_campaign_allows_delivery_ack_but_not_new_fulfillment() {
        assert!(
            validate_beacon_release_recipient_transition("sent", "delivered", "closed").is_ok()
        );
        assert_eq!(
            validate_beacon_release_recipient_transition("prepared", "sent", "closed"),
            Err(BeaconReleaseTransitionError::Conflict),
        );
    }

    #[test]
    fn activation_copy_is_engagement_only_and_contains_no_shipping_prompt() {
        let copy = beacon_release_activation_copy(
            "pl-PL",
            "Radio Test",
            "Echoes",
            "https://virya.music/pl/latarnik/#wydania",
        );
        assert!(copy.subject.contains("Echoes"));
        assert!(copy.text.contains("Press Room"));
        assert!(!copy.text.contains("Paczkomat"));
        assert!(!copy.text.to_lowercase().contains("telefon"));
    }
}
