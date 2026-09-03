//! Durable, signed ticket QR credentials.
//!
//! Rotating Signal/winner QR codes use the short-lived `v1` format owned by
//! `admission.rs`. Paid tickets need a stable credential that can be delivered
//! by e-mail and printed, so this module uses a separate `t1` prefix and an
//! explicit gate validity window. The database remains authoritative: a valid
//! signature never overrides a revoked, refunded, expired, or already redeemed
//! pass.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_domain::{AdmissionPassId, AdmissionQrError, EventId};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const TOKEN_PREFIX: &str = "t1";
const MAX_PAYLOAD_BYTES: usize = 512;
const MAX_VALIDITY_SECONDS: i64 = 3 * 366 * 24 * 60 * 60;
const CLOCK_SKEW_SECONDS: i64 = 5;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TicketQrClaims {
    #[serde(rename = "v")]
    version: u8,
    #[serde(rename = "p")]
    pass_id: Uuid,
    #[serde(rename = "e")]
    event_id: Uuid,
    #[serde(rename = "r")]
    public_reference: String,
    #[serde(rename = "n")]
    not_before: i64,
    #[serde(rename = "x")]
    expires_at: i64,
}

/// Identity recovered from a verified durable ticket QR.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TicketQrIdentity {
    pub pass_id: AdmissionPassId,
    pub event_id: EventId,
    pub public_reference: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TicketQrEncodingError {
    InvalidClaims,
    Serialization,
    InvalidSigningKey,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_ticket_qr(
    pass_id: Uuid,
    event_id: Uuid,
    public_reference: &str,
    not_before: i64,
    expires_at: i64,
    key: &[u8; 32],
) -> Result<String, TicketQrEncodingError> {
    let claims = TicketQrClaims {
        version: 1,
        pass_id,
        event_id,
        public_reference: public_reference.to_owned(),
        not_before,
        expires_at,
    };
    validate_claims(&claims, not_before).map_err(|_| TicketQrEncodingError::InvalidClaims)?;
    let payload = serde_json::to_vec(&claims).map_err(|_| TicketQrEncodingError::Serialization)?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(TicketQrEncodingError::InvalidClaims);
    }
    let encoded = URL_SAFE_NO_PAD.encode(payload);
    let signed = format!("{TOKEN_PREFIX}.{encoded}");
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| TicketQrEncodingError::InvalidSigningKey)?;
    mac.update(signed.as_bytes());
    Ok(format!(
        "{signed}.{}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

pub(crate) fn decode_ticket_qr(
    token: &str,
    key: &[u8; 32],
    now: i64,
) -> Result<TicketQrIdentity, AdmissionQrError> {
    let mut parts = token.split('.');
    let prefix = parts.next().ok_or(AdmissionQrError::Invalid)?;
    let payload = parts.next().ok_or(AdmissionQrError::Invalid)?;
    let signature = parts.next().ok_or(AdmissionQrError::Invalid)?;
    if prefix != TOKEN_PREFIX || parts.next().is_some() || payload.len() > 1_024 {
        return Err(AdmissionQrError::Invalid);
    }

    let signature = hex::decode(signature).map_err(|_| AdmissionQrError::Invalid)?;
    let signed = format!("{prefix}.{payload}");
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| AdmissionQrError::Invalid)?;
    mac.update(signed.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| AdmissionQrError::Invalid)?;

    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AdmissionQrError::Invalid)?;
    if decoded.len() > MAX_PAYLOAD_BYTES {
        return Err(AdmissionQrError::Invalid);
    }
    let claims: TicketQrClaims =
        serde_json::from_slice(&decoded).map_err(|_| AdmissionQrError::Invalid)?;
    validate_claims(&claims, now)?;

    Ok(TicketQrIdentity {
        pass_id: AdmissionPassId::from_uuid(claims.pass_id),
        event_id: EventId::from_uuid(claims.event_id),
        public_reference: claims.public_reference,
    })
}

fn validate_claims(claims: &TicketQrClaims, now: i64) -> Result<(), AdmissionQrError> {
    let Some(lifetime) = claims.expires_at.checked_sub(claims.not_before) else {
        return Err(AdmissionQrError::Invalid);
    };
    if claims.version != 1
        || claims.public_reference.is_empty()
        || claims.public_reference.len() > 64
        || !claims
            .public_reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || claims.expires_at <= claims.not_before
        || lifetime > MAX_VALIDITY_SECONDS
        || now.saturating_add(CLOCK_SKEW_SECONDS) < claims.not_before
    {
        return Err(AdmissionQrError::Invalid);
    }
    if now.saturating_sub(CLOCK_SKEW_SECONDS) > claims.expires_at {
        return Err(AdmissionQrError::Expired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_ticket_qr_round_trip_and_window_validation() {
        let key = [11_u8; 32];
        let pass_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let token = encode_ticket_qr(pass_id, event_id, "VIRYA-ABC123", 1_000, 2_000, &key)
            .expect("ticket QR should encode");
        let identity = decode_ticket_qr(&token, &key, 1_500).expect("ticket QR should decode");
        assert_eq!(identity.pass_id.into_uuid(), pass_id);
        assert_eq!(identity.event_id.into_uuid(), event_id);
        assert_eq!(identity.public_reference, "VIRYA-ABC123");
        assert_eq!(
            decode_ticket_qr(&token, &key, 900),
            Err(AdmissionQrError::Invalid)
        );
        assert_eq!(
            decode_ticket_qr(&token, &key, 2_100),
            Err(AdmissionQrError::Expired)
        );
        assert_eq!(
            decode_ticket_qr(&(token + "x"), &key, 1_500),
            Err(AdmissionQrError::Invalid)
        );
    }
}
