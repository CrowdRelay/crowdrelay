//! Fan email-confirmation and unsubscribe lifecycle types.
//!
//! Defines the opaque action token used for confirmation and unsubscribe
//! links, and the result types returned after each lifecycle transition.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;

use crate::{FanId, FanSessionToken, FanStatus, ReferralCode};

/// Opaque one-time token used for confirmation or unsubscribe actions.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FanActionToken(String);

impl FanActionToken {
    /// Parses a 256-bit hexadecimal lifecycle token.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, FanActionTokenError> {
        let value = value.as_ref();
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(FanActionTokenError);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the normalized token representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for FanActionToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Debug for FanActionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FanActionToken")
            .field(&"[REDACTED]")
            .finish()
    }
}

/// Error returned for malformed lifecycle tokens.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("fan action token must contain exactly 64 hexadecimal characters")]
pub struct FanActionTokenError;

/// Result returned after successful email ownership confirmation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FanConfirmationResult {
    pub fan_id: FanId,
    pub status: FanStatus,
    pub referral_code: ReferralCode,
    pub fan_session_token: FanSessionToken,
}

/// Result returned after an unsubscribe action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FanUnsubscribeResult {
    pub fan_id: FanId,
    pub status: FanStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_tokens_are_strict_and_redacted() -> Result<(), Box<dyn std::error::Error>> {
        let token = FanActionToken::parse("B".repeat(64))?;
        assert_eq!(token.as_str(), "b".repeat(64));
        assert!(!format!("{token:?}").contains(token.as_str()));
        assert!(FanActionToken::parse("short").is_err());
        Ok(())
    }
}
