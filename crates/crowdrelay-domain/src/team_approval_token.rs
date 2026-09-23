//! One-click team-approval links (§6-C).
//!
//! The daily briefing and `team.assignment.email` payloads carry a signed URL
//! per pending ask, so a crew member can approve or skip from the e-mail
//! without a panel session. The token is a compact JWS-shaped pair —
//! `base64url(payload) "." base64url(mac)` — where the MAC covers a fixed
//! purpose string and the payload, so a token minted anywhere else in the
//! system verifies against nothing here.
//!
//! The verdict is deliberately *not* in the claims: the same link serves both
//! buttons, and the POST body's `verdict` field carries the choice.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

/// Domain separation for token signatures — the MAC input, so a token minted
/// for any other subsystem cannot be replayed here.
pub const TEAM_APPROVAL_TOKEN_PURPOSE: &str = "crowdrelay.team_approval.v1";

/// Domain separation for the key itself: the configured secret is hashed with
/// this string so the same secret used for attestations or response replay
/// yields an unrelated key here — same derivation pattern as
/// `AttestationSigningKey`.
const KEY_DERIVATION_DOMAIN: &[u8] = b"crowdrelay.team_approval.signing.v1\n";

/// The server-held key an approval link is signed under.
///
/// Debug is redacted for the same reason `AttestationSigningKey`'s is: a key
/// that reaches a log has to be rotated, and the rotation invalidates every
/// link already mailed.
#[derive(Clone)]
pub struct TeamApprovalKey([u8; 32]);

impl std::fmt::Debug for TeamApprovalKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TeamApprovalKey([REDACTED])")
    }
}

impl PartialEq for TeamApprovalKey {
    fn eq(&self, other: &Self) -> bool {
        let mut difference = 0u8;
        for (left, right) in self.0.iter().zip(other.0.iter()) {
            difference |= left ^ right;
        }
        difference == 0
    }
}

impl Eq for TeamApprovalKey {}

impl TeamApprovalKey {
    #[must_use]
    pub fn derive_from_secret(secret: &[u8]) -> Self {
        let mut digest = <Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut digest, KEY_DERIVATION_DOMAIN);
        sha2::Digest::update(&mut digest, secret);
        Self(sha2::Digest::finalize(digest).into())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalTokenClaims {
    pub action_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<Uuid>,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ApprovalTokenError {
    #[error("the token is not shaped like an approval token")]
    Malformed,
    #[error("the token's signature does not verify")]
    BadSignature,
    #[error("the token's window has closed")]
    Expired,
}

fn mac(key: &[u8; 32], payload: &[u8]) -> [u8; 32] {
    // HMAC accepts any key length and this one is fixed at 32 bytes, so
    // initialisation cannot fail — but the crate denies `expect_used`, and an
    // empty MAC compared against an empty presented MAC would verify. A zero
    // MAC can only collide with a presented zero MAC, and the payload it
    // would have signed cannot exist because signing never produced one.
    let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(key) else {
        return [0u8; 32];
    };
    mac.update(TEAM_APPROVAL_TOKEN_PURPOSE.as_bytes());
    mac.update(payload);
    mac.finalize().into_bytes().into()
}

#[must_use]
pub fn encode(claims: &ApprovalTokenClaims, key: &TeamApprovalKey) -> String {
    let payload = serde_json::to_vec(claims).unwrap_or_default();
    let signature = mac(&key.0, &payload);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(signature)
    )
}

pub fn decode(
    token: &str,
    key: &TeamApprovalKey,
    now: OffsetDateTime,
) -> Result<ApprovalTokenClaims, ApprovalTokenError> {
    let (payload_b64, signature_b64) =
        token.split_once('.').ok_or(ApprovalTokenError::Malformed)?;
    if signature_b64.contains('.') {
        return Err(ApprovalTokenError::Malformed);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| ApprovalTokenError::Malformed)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature_b64)
        .map_err(|_| ApprovalTokenError::Malformed)?;
    let expected = mac(&key.0, &payload);
    if signature.len() != expected.len() {
        return Err(ApprovalTokenError::BadSignature);
    }
    // Byte-wise OR accumulate: an early return leaks how many leading bytes
    // were right, and the endpoint this guards is public.
    let mut difference = 0u8;
    for (left, right) in expected.iter().zip(signature.iter()) {
        difference |= left ^ right;
    }
    if difference != 0 {
        return Err(ApprovalTokenError::BadSignature);
    }
    let claims: ApprovalTokenClaims =
        serde_json::from_slice(&payload).map_err(|_| ApprovalTokenError::Malformed)?;
    if claims.expires_at <= now {
        return Err(ApprovalTokenError::Expired);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn key() -> TeamApprovalKey {
        TeamApprovalKey::derive_from_secret(b"a-team-approval-test-secret")
    }

    fn claims() -> ApprovalTokenClaims {
        ApprovalTokenClaims {
            action_id: Uuid::from_u128(0xaaaa),
            assignment_id: Some(Uuid::from_u128(0xbbbb)),
            expires_at: datetime!(2030-01-01 00:00 UTC),
        }
    }

    #[test]
    fn a_token_round_trips() {
        let token = encode(&claims(), &key());
        let decoded =
            decode(&token, &key(), datetime!(2026-01-01 00:00 UTC)).expect("a fresh token decodes");
        assert_eq!(decoded, claims());
    }

    #[test]
    fn a_tampered_payload_fails_signature() {
        let token = encode(&claims(), &key());
        let (payload, signature) = token.split_once('.').expect("two parts");
        let mut forged = claims();
        forged.action_id = Uuid::from_u128(0xdead);
        let forged_token = format!(
            "{}.{signature}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).expect("json"))
        );
        let _ = payload; // the forged token carries a different payload
        assert_eq!(
            decode(&forged_token, &key(), datetime!(2026-01-01 00:00 UTC)),
            Err(ApprovalTokenError::BadSignature)
        );
    }

    #[test]
    fn a_tampered_signature_fails() {
        let token = encode(&claims(), &key());
        let (payload, _) = token.split_once('.').expect("two parts");
        let forged = format!("{payload}.{}", URL_SAFE_NO_PAD.encode([9u8; 32]));
        assert_eq!(
            decode(&forged, &key(), datetime!(2026-01-01 00:00 UTC)),
            Err(ApprovalTokenError::BadSignature)
        );
    }

    #[test]
    fn an_expired_token_says_so() {
        let expired = ApprovalTokenClaims {
            expires_at: datetime!(2020-01-01 00:00 UTC),
            ..claims()
        };
        let token = encode(&expired, &key());
        assert_eq!(
            decode(&token, &key(), datetime!(2026-01-01 00:00 UTC)),
            Err(ApprovalTokenError::Expired)
        );
    }

    #[test]
    fn a_token_signed_for_another_purpose_is_rejected() {
        // The same key, a different purpose string: exactly the replay the
        // purpose prefix exists to stop.
        let payload = serde_json::to_vec(&claims()).expect("json");
        let mut mac =
            <Hmac<Sha256> as KeyInit>::new_from_slice(&key().0).expect("fixed 32-byte key");
        mac.update(b"crowdrelay.attestation.v1");
        mac.update(&payload);
        let signature = mac.finalize().into_bytes();
        let token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(signature)
        );
        assert_eq!(
            decode(&token, &key(), datetime!(2026-01-01 00:00 UTC)),
            Err(ApprovalTokenError::BadSignature)
        );
    }

    #[test]
    fn a_malformed_token_says_so() {
        for bad in ["", "no-dot", "a.b.c", "!!!.AAA", "aGVsbG8.not-base64!!"] {
            assert_eq!(
                decode(bad, &key(), datetime!(2026-01-01 00:00 UTC)),
                Err(ApprovalTokenError::Malformed),
                "{bad:?} should be malformed"
            );
        }
    }
}
