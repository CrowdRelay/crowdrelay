//! Person-level FAN SCOUT primitives.
//!
//! A public person is not a fan merely because CrowdRelay can see them.
//! This module keeps that boundary structural: a prospect has a public
//! platform identity and evidence; becoming a first-party fan is a separate,
//! later link to an already verified fan record.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// How a public platform identifies one person.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FanProspectIdentityKind {
    /// Stable provider/user/channel id. Case is significant.
    PlatformUserId,
    /// Public username/handle. Canonicalized case-insensitively for dedupe.
    Handle,
}

impl FanProspectIdentityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlatformUserId => "platform_user_id",
            Self::Handle => "handle",
        }
    }
}

/// Lifecycle state of a person who is not necessarily in the owned fanbase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FanProspectStatus {
    Observed,
    Qualified,
    Warming,
    Invited,
    Converted,
    Held,
    Refused,
    Suppressed,
}

impl FanProspectStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Qualified => "qualified",
            Self::Warming => "warming",
            Self::Invited => "invited",
            Self::Converted => "converted",
            Self::Held => "held",
            Self::Refused => "refused",
            Self::Suppressed => "suppressed",
        }
    }

    /// States that discovery must never silently undo.
    #[must_use]
    pub const fn blocks_conversion(self) -> bool {
        matches!(self, Self::Refused | Self::Suppressed)
    }
}

/// What one sourced observation says about a prospect.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FanProspectObservationKind {
    Discovery,
    PublicEngagement,
    Affinity,
    Intent,
    Locality,
    ReferralPotential,
    Network,
    Contactability,
    NegativeSignal,
}

impl FanProspectObservationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::PublicEngagement => "public_engagement",
            Self::Affinity => "affinity",
            Self::Intent => "intent",
            Self::Locality => "locality",
            Self::ReferralPotential => "referral_potential",
            Self::Network => "network",
            Self::Contactability => "contactability",
            Self::NegativeSignal => "negative_signal",
        }
    }
}

/// A validated platform identity plus the key used for dedupe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanProspectIdentity {
    platform: String,
    kind: FanProspectIdentityKind,
    external_identity: String,
    identity_key: String,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum FanProspectIdentityError {
    #[error("platform must be 1-32 lowercase ASCII letters, digits, '_' or '-'")]
    InvalidPlatform,
    #[error("external identity must be 1-256 printable characters")]
    InvalidExternalIdentity,
}

impl FanProspectIdentity {
    /// Validates and canonicalizes one public identity.
    ///
    /// Handles are case-insensitive and may arrive with '@'; provider ids are
    /// preserved byte-for-byte because some provider ids are case-sensitive.
    ///
    /// # Errors
    /// Invalid platform or external identity.
    pub fn new(
        platform: &str,
        kind: FanProspectIdentityKind,
        external_identity: &str,
    ) -> Result<Self, FanProspectIdentityError> {
        let platform = platform.trim();
        let platform_ok = !platform.is_empty()
            && platform.len() <= 32
            && platform.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'-')
            });
        if !platform_ok {
            return Err(FanProspectIdentityError::InvalidPlatform);
        }

        let raw = external_identity.trim();
        let raw = match kind {
            FanProspectIdentityKind::Handle => raw.strip_prefix('@').unwrap_or(raw),
            FanProspectIdentityKind::PlatformUserId => raw,
        };
        let len = raw.chars().count();
        if len == 0 || len > 256 || raw.chars().any(char::is_control) {
            return Err(FanProspectIdentityError::InvalidExternalIdentity);
        }
        let external_identity = raw.to_owned();
        let identity_key = match kind {
            FanProspectIdentityKind::Handle => raw.to_lowercase(),
            FanProspectIdentityKind::PlatformUserId => raw.to_owned(),
        };
        if identity_key.chars().count() > 256 {
            return Err(FanProspectIdentityError::InvalidExternalIdentity);
        }
        Ok(Self {
            platform: platform.to_owned(),
            kind,
            external_identity,
            identity_key,
        })
    }

    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    #[must_use]
    pub const fn kind(&self) -> FanProspectIdentityKind {
        self.kind
    }

    #[must_use]
    pub fn external_identity(&self) -> &str {
        &self.external_identity
    }

    #[must_use]
    pub fn identity_key(&self) -> &str {
        &self.identity_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_dedupe_case_and_at_sign_without_rewriting_display_identity() {
        let identity = FanProspectIdentity::new(
            "instagram",
            FanProspectIdentityKind::Handle,
            " @MetalFanPL ",
        )
        .expect("valid handle");
        assert_eq!(identity.external_identity(), "MetalFanPL");
        assert_eq!(identity.identity_key(), "metalfanpl");
    }

    #[test]
    fn provider_ids_keep_case_because_some_platform_ids_are_case_sensitive() {
        let identity = FanProspectIdentity::new(
            "youtube",
            FanProspectIdentityKind::PlatformUserId,
            "UCaBcD123",
        )
        .expect("valid provider id");
        assert_eq!(identity.external_identity(), "UCaBcD123");
        assert_eq!(identity.identity_key(), "UCaBcD123");
    }

    #[test]
    fn refusal_and_suppression_are_terminal_for_conversion() {
        assert!(FanProspectStatus::Refused.blocks_conversion());
        assert!(FanProspectStatus::Suppressed.blocks_conversion());
        assert!(!FanProspectStatus::Observed.blocks_conversion());
    }

    #[test]
    fn platform_names_are_deliberately_strict() {
        assert!(
            FanProspectIdentity::new(
                "Instagram",
                FanProspectIdentityKind::Handle,
                "fan",
            )
            .is_err()
        );
        assert!(
            FanProspectIdentity::new(
                "instagram",
                FanProspectIdentityKind::Handle,
                "@",
            )
            .is_err()
        );
    }
}
