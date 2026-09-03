use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use thiserror::Error;

use super::SecretValue;

/// Computes the protocol-v1 signature over `timestamp + "." + exact_raw_body`.
pub fn sign_webhook(
    secret: &SecretValue,
    timestamp: i64,
    exact_raw_body: &[u8],
) -> Result<String, WebhookSignatureError> {
    let mut signer = Hmac::<Sha256>::new_from_slice(secret.expose())
        .map_err(|_| WebhookSignatureError::InvalidKeyMaterial)?;
    signer.update(timestamp.to_string().as_bytes());
    signer.update(b".");
    signer.update(exact_raw_body);
    Ok(hex::encode(signer.finalize().into_bytes()))
}

/// Sanitized signing failure that never exposes key material.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum WebhookSignatureError {
    /// The signing key material was invalid or empty.
    #[error("webhook signing key material is invalid")]
    InvalidKeyMaterial,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_timestamp_dot_exact_body() -> Result<(), Box<dyn std::error::Error>> {
        let secret = SecretValue::new(b"0123456789abcdef0123456789abcdef".to_vec())?;
        let signature = sign_webhook(&secret, 1_785_240_000, br#"{"type":"fan.created"}"#)?;

        assert_eq!(
            signature,
            "2d25b5bcd1efc98f23b74f4acd008d2a96086a76880123e8634a90971aa9a5ec"
        );
        assert_ne!(
            signature,
            sign_webhook(&secret, 1_785_240_000, br#"{ "type":"fan.created" }"#)?
        );
        Ok(())
    }
}
