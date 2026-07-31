//! Admission-pass state, claim tokens, rotating QR payloads, and redemption results.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;
use time::OffsetDateTime;

use crate::{AdmissionPassId, EventId, FanId, PassSessionId};

/// Current lifecycle state of a free-admission pass.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionPassStatus {
    /// Pass has been issued but not yet claimed by the winner.
    Issued,
    /// Winner has exchanged the one-time claim token for a browser session.
    Claimed,
    /// Pass has been redeemed at the gate.
    Redeemed,
    /// Pass has been administratively revoked.
    Revoked,
    /// Pass has passed its expiry deadline without redemption.
    Expired,
}

/// Opaque one-time token delivered to the pass recipient.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PassClaimToken(String);

impl PassClaimToken {
    /// Parses a 256-bit hexadecimal pass-claim token.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, PassClaimTokenError> {
        let value = value.as_ref();
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PassClaimTokenError);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the normalized token representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PassClaimToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Debug for PassClaimToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PassClaimToken")
            .field(&"[REDACTED]")
            .finish()
    }
}

/// Error returned for malformed pass-claim tokens.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("pass claim token must contain exactly 64 hexadecimal characters")]
pub struct PassClaimTokenError;

/// Opaque browser session issued after a winner claims a pass.
#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PassSessionToken(String);

impl PassSessionToken {
    /// Parses a 256-bit hexadecimal pass-session token.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, PassSessionTokenError> {
        let value = value.as_ref();
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PassSessionTokenError);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the normalized token representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PassSessionToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Debug for PassSessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PassSessionToken")
            .field(&"[REDACTED]")
            .finish()
    }
}

/// Error returned for malformed pass-session tokens.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("pass session token must contain exactly 64 hexadecimal characters")]
pub struct PassSessionTokenError;

/// Public winner view returned after a successful pass claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdmissionPassView {
    pub pass_id: AdmissionPassId,
    pub session_id: Option<PassSessionId>,
    pub event_id: EventId,
    pub event_slug: String,
    pub event_title: String,
    pub venue: Option<String>,
    pub starts_at: OffsetDateTime,
    pub holder_name: Option<String>,
    pub holder_email_masked: String,
    pub public_reference: String,
    pub status: AdmissionPassStatus,
    pub session_expires_at: OffsetDateTime,
    pub redeemed_at: Option<OffsetDateTime>,
}

/// Result returned when an operator issues a pass from a limited pool.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdmissionPassIssued {
    pub pass_id: AdmissionPassId,
    pub event_id: EventId,
    pub fan_id: FanId,
    pub public_reference: String,
    pub claim_token: PassClaimToken,
    pub claim_expires_at: OffsetDateTime,
    pub created: bool,
}

/// Result returned after a winner exchanges a one-time claim token.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdmissionPassClaimed {
    pub pass: AdmissionPassView,
    pub session_token: PassSessionToken,
}

/// Signed, short-lived payload embedded in a winner QR code.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdmissionQrClaims {
    pub version: u8,
    pub pass_id: AdmissionPassId,
    pub event_id: EventId,
    pub public_reference: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub nonce: String,
}

impl AdmissionQrClaims {
    /// Validates structural and lifetime constraints before signing or accepting claims.
    pub fn validate(
        &self,
        now: i64,
        maximum_lifetime_seconds: i64,
    ) -> Result<(), AdmissionQrError> {
        let Some(lifetime_seconds) = self.expires_at.checked_sub(self.issued_at) else {
            return Err(AdmissionQrError::Invalid);
        };
        if self.version != 1
            || self.public_reference.is_empty()
            || self.public_reference.len() > 64
            || !self
                .public_reference
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || self.nonce.len() != 36
            || self.expires_at <= self.issued_at
            || lifetime_seconds > maximum_lifetime_seconds
        {
            return Err(AdmissionQrError::Invalid);
        }
        if now > self.expires_at {
            return Err(AdmissionQrError::Expired);
        }
        Ok(())
    }
}

/// Error returned when a rotating admission QR is invalid or expired.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum AdmissionQrError {
    /// The QR payload failed structural or lifetime validation.
    #[error("admission QR payload is invalid")]
    Invalid,
    /// The QR payload has passed its expiry time.
    #[error("admission QR payload has expired")]
    Expired,
}

/// Outcome of an atomic gate redemption operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdmissionRedemptionResult {
    pub pass_id: AdmissionPassId,
    pub event_id: EventId,
    pub public_reference: String,
    pub holder_name: Option<String>,
    pub holder_email_masked: String,
    pub status: AdmissionRedemptionStatus,
    pub redeemed_at: Option<OffsetDateTime>,
}

/// Gate result shown to staff immediately after scanning a pass.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionRedemptionStatus {
    /// Pass was successfully redeemed in this transaction.
    Redeemed,
    /// Pass was already redeemed in a previous transaction.
    AlreadyRedeemed,
    /// Pass has been revoked and cannot be redeemed.
    Revoked,
    /// Pass has expired and cannot be redeemed.
    Expired,
    /// Pass has not been claimed by the winner.
    NotClaimed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_tokens_are_normalized_and_redacted() -> Result<(), Box<dyn std::error::Error>> {
        let token = PassClaimToken::parse("A".repeat(64))?;
        assert_eq!(token.as_str(), "a".repeat(64));
        assert!(!format!("{token:?}").contains(token.as_str()));
        assert!(PassSessionToken::parse("not-a-token").is_err());
        Ok(())
    }

    #[test]
    fn qr_claims_reject_expired_and_overlong_windows() {
        let claims = AdmissionQrClaims {
            version: 1,
            pass_id: AdmissionPassId::new(),
            event_id: EventId::new(),
            public_reference: "VIRYA-ABC123".to_owned(),
            issued_at: 100,
            expires_at: 130,
            nonce: "00000000-0000-7000-8000-000000000000".to_owned(),
        };
        assert!(claims.validate(110, 60).is_ok());
        assert_eq!(claims.validate(131, 60), Err(AdmissionQrError::Expired));
        assert_eq!(claims.validate(110, 20), Err(AdmissionQrError::Invalid));

        let overflowing_lifetime = AdmissionQrClaims {
            issued_at: i64::MIN,
            expires_at: i64::MAX,
            ..claims
        };
        assert_eq!(
            overflowing_lifetime.validate(0, i64::MAX),
            Err(AdmissionQrError::Invalid)
        );
    }
}
