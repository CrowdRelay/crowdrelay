//! Authenticated encryption for sensitive responses retained for idempotent replay.
//!
//! The JSON envelope is intentionally self-describing and versioned, while the
//! workspace, operation scope, and idempotency key are authenticated as
//! associated data rather than persisted inside the ciphertext.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    Key, XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use crowdrelay_domain::WorkspaceId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::Zeroize;

const ENVELOPE_VERSION: u8 = 1;
const ALGORITHM: &str = "XChaCha20-Poly1305";
const KEY_DERIVATION_DOMAIN: &[u8] = b"crowdrelay.response-encryption.key.v1\0";
const ASSOCIATED_DATA_DOMAIN: &[u8] = b"crowdrelay.sensitive-idempotency-response.v1\0";
const XCHACHA20_NONCE_BYTES: usize = 24;

/// A derived 256-bit key used only for sensitive idempotency responses.
///
/// External secret validation belongs to configuration parsing. This type
/// domain-separates and hashes the configured secret so its raw representation
/// is never retained in runtime configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct SensitiveResponseKey([u8; 32]);

impl SensitiveResponseKey {
    /// Derives a response-encryption key from high-entropy secret material.
    #[must_use]
    pub fn derive_from_secret(secret: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(KEY_DERIVATION_DOMAIN);
        digest.update(secret);
        Self(digest.finalize().into())
    }

    fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SensitiveResponseKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveResponseKey([REDACTED])")
    }
}

impl Drop for SensitiveResponseKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Shared codec for encrypted JSON responses stored in PostgreSQL JSONB.
#[derive(Clone)]
pub struct SensitiveResponseCodec {
    key: SensitiveResponseKey,
    previous_key: Option<SensitiveResponseKey>,
}

impl SensitiveResponseCodec {
    /// Creates a codec backed by the supplied derived key.
    #[must_use]
    pub fn new(key: SensitiveResponseKey) -> Self {
        Self {
            key,
            previous_key: None,
        }
    }

    /// Creates a codec that encrypts with `key` and can decrypt responses
    /// written with one immediately preceding deployment key.
    #[must_use]
    pub fn with_previous_key(
        key: SensitiveResponseKey,
        previous_key: Option<SensitiveResponseKey>,
    ) -> Self {
        Self { key, previous_key }
    }

    /// Encrypts a serializable response into a versioned JSON envelope.
    pub fn encrypt<T: Serialize>(
        &self,
        workspace_id: WorkspaceId,
        scope: &str,
        idempotency_key: &str,
        response: &T,
    ) -> Result<Value, SensitiveResponseError> {
        let mut plaintext =
            serde_json::to_vec(response).map_err(|_| SensitiveResponseError::Serialization)?;
        let mut nonce_bytes = [0_u8; XCHACHA20_NONCE_BYTES];
        if getrandom::fill(&mut nonce_bytes).is_err() {
            plaintext.zeroize();
            return Err(SensitiveResponseError::Randomness);
        }

        let associated_data = associated_data(workspace_id, scope, idempotency_key)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(self.key.expose()));
        let encrypted = cipher.encrypt(
            XNonce::from_slice(&nonce_bytes),
            Payload {
                msg: &plaintext,
                aad: &associated_data,
            },
        );
        plaintext.zeroize();
        let ciphertext = encrypted.map_err(|_| SensitiveResponseError::Encryption)?;

        serde_json::to_value(EncryptedEnvelope {
            version: ENVELOPE_VERSION,
            algorithm: ALGORITHM.to_owned(),
            nonce: URL_SAFE_NO_PAD.encode(nonce_bytes),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        })
        .map_err(|_| SensitiveResponseError::Serialization)
    }

    /// Authenticates and decrypts a versioned JSON envelope.
    pub fn decrypt<T: DeserializeOwned>(
        &self,
        workspace_id: WorkspaceId,
        scope: &str,
        idempotency_key: &str,
        envelope: Value,
    ) -> Result<T, SensitiveResponseError> {
        let envelope: EncryptedEnvelope =
            serde_json::from_value(envelope).map_err(|_| SensitiveResponseError::Malformed)?;
        if envelope.version != ENVELOPE_VERSION {
            return Err(SensitiveResponseError::UnsupportedVersion);
        }
        if envelope.algorithm != ALGORITHM {
            return Err(SensitiveResponseError::UnsupportedAlgorithm);
        }

        let nonce = URL_SAFE_NO_PAD
            .decode(envelope.nonce)
            .map_err(|_| SensitiveResponseError::Malformed)?;
        let nonce: [u8; XCHACHA20_NONCE_BYTES] = nonce
            .try_into()
            .map_err(|_| SensitiveResponseError::Malformed)?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(envelope.ciphertext)
            .map_err(|_| SensitiveResponseError::Malformed)?;
        let associated_data = associated_data(workspace_id, scope, idempotency_key)?;
        let decrypt = |key: &SensitiveResponseKey| {
            XChaCha20Poly1305::new(Key::from_slice(key.expose())).decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &associated_data,
                },
            )
        };
        let mut plaintext = match decrypt(&self.key) {
            Ok(plaintext) => plaintext,
            Err(_) => self
                .previous_key
                .as_ref()
                .and_then(|key| decrypt(key).ok())
                .ok_or(SensitiveResponseError::Authentication)?,
        };
        let response =
            serde_json::from_slice(&plaintext).map_err(|_| SensitiveResponseError::Serialization);
        plaintext.zeroize();
        response
    }
}

impl fmt::Debug for SensitiveResponseCodec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SensitiveResponseCodec")
            .field("key", &"[REDACTED]")
            .field(
                "previous_key",
                &self.previous_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct EncryptedEnvelope {
    #[serde(rename = "v")]
    version: u8,
    #[serde(rename = "alg")]
    algorithm: String,
    nonce: String,
    ciphertext: String,
}

fn associated_data(
    workspace_id: WorkspaceId,
    scope: &str,
    idempotency_key: &str,
) -> Result<Vec<u8>, SensitiveResponseError> {
    let scope_length =
        u64::try_from(scope.len()).map_err(|_| SensitiveResponseError::ContextTooLarge)?;
    let key_length = u64::try_from(idempotency_key.len())
        .map_err(|_| SensitiveResponseError::ContextTooLarge)?;
    let mut data = Vec::with_capacity(
        ASSOCIATED_DATA_DOMAIN.len()
            + 16
            + size_of::<u64>()
            + scope.len()
            + size_of::<u64>()
            + idempotency_key.len(),
    );
    data.extend_from_slice(ASSOCIATED_DATA_DOMAIN);
    data.extend_from_slice(workspace_id.into_uuid().as_bytes());
    data.extend_from_slice(&scope_length.to_be_bytes());
    data.extend_from_slice(scope.as_bytes());
    data.extend_from_slice(&key_length.to_be_bytes());
    data.extend_from_slice(idempotency_key.as_bytes());
    Ok(data)
}

/// Failure while encoding or decoding a sensitive response.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SensitiveResponseError {
    /// JSON encoding or decoding failed.
    #[error("sensitive response serialization failed")]
    Serialization,
    /// The operating system could not provide a nonce.
    #[error("secure random nonce generation failed")]
    Randomness,
    /// The authenticated encryption operation failed.
    #[error("sensitive response encryption failed")]
    Encryption,
    /// The stored value was not a valid encrypted envelope.
    #[error("sensitive response envelope is malformed")]
    Malformed,
    /// The envelope version is not supported by this binary.
    #[error("sensitive response envelope version is unsupported")]
    UnsupportedVersion,
    /// The envelope encryption algorithm is not supported by this binary.
    #[error("sensitive response envelope algorithm is unsupported")]
    UnsupportedAlgorithm,
    /// Authentication failed because the envelope or its context changed.
    #[error("sensitive response authentication failed")]
    Authentication,
    /// Associated-data fields exceeded the supported representation.
    #[error("sensitive response context is too large")]
    ContextTooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: &str = "admission.pass.issue";
    const KEY: &str = "issue-pass-test-0001";
    const CLAIM_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct SensitiveResult {
        pass_id: String,
        claim_token: String,
    }

    fn codec() -> SensitiveResponseCodec {
        SensitiveResponseCodec::new(SensitiveResponseKey::derive_from_secret(
            b"unit-test-response-secret-never-use-in-production",
        ))
    }

    fn result() -> SensitiveResult {
        SensitiveResult {
            pass_id: "pass-123".to_owned(),
            claim_token: CLAIM_TOKEN.to_owned(),
        }
    }

    #[test]
    fn encrypts_round_trip_without_plaintext_in_envelope() -> Result<(), Box<dyn std::error::Error>>
    {
        let codec = codec();
        let workspace_id = WorkspaceId::new();
        let encrypted = codec.encrypt(workspace_id, SCOPE, KEY, &result())?;

        assert!(!encrypted.to_string().contains(CLAIM_TOKEN));
        assert_eq!(
            codec.decrypt::<SensitiveResult>(workspace_id, SCOPE, KEY, encrypted)?,
            result()
        );
        Ok(())
    }

    #[test]
    fn uses_a_fresh_nonce_for_every_envelope() -> Result<(), Box<dyn std::error::Error>> {
        let codec = codec();
        let workspace_id = WorkspaceId::new();

        let first = codec.encrypt(workspace_id, SCOPE, KEY, &result())?;
        let second = codec.encrypt(workspace_id, SCOPE, KEY, &result())?;

        assert_ne!(first, second);
        Ok(())
    }

    #[test]
    fn rejects_ciphertext_tampering() -> Result<(), Box<dyn std::error::Error>> {
        let codec = codec();
        let workspace_id = WorkspaceId::new();
        let encrypted = codec.encrypt(workspace_id, SCOPE, KEY, &result())?;
        let mut envelope: EncryptedEnvelope = serde_json::from_value(encrypted)?;
        let mut ciphertext = URL_SAFE_NO_PAD.decode(&envelope.ciphertext)?;
        let first = ciphertext
            .first_mut()
            .ok_or("encrypted payload must not be empty")?;
        *first ^= 1;
        envelope.ciphertext = URL_SAFE_NO_PAD.encode(ciphertext);

        let error = codec
            .decrypt::<SensitiveResult>(workspace_id, SCOPE, KEY, serde_json::to_value(envelope)?)
            .expect_err("tampered ciphertext must be rejected");
        assert_eq!(error, SensitiveResponseError::Authentication);
        Ok(())
    }

    #[test]
    fn binds_workspace_scope_and_idempotency_key_as_aad() -> Result<(), Box<dyn std::error::Error>>
    {
        let codec = codec();
        let workspace_id = WorkspaceId::new();
        let encrypted = codec.encrypt(workspace_id, SCOPE, KEY, &result())?;

        for (other_workspace, other_scope, other_key) in [
            (WorkspaceId::new(), SCOPE, KEY),
            (workspace_id, "admission.pass.revoke", KEY),
            (workspace_id, SCOPE, "issue-pass-test-0002"),
        ] {
            assert_eq!(
                codec
                    .decrypt::<SensitiveResult>(
                        other_workspace,
                        other_scope,
                        other_key,
                        encrypted.clone(),
                    )
                    .expect_err("changed associated data must be rejected"),
                SensitiveResponseError::Authentication
            );
        }
        Ok(())
    }

    #[test]
    fn decrypts_with_one_previous_key_but_encrypts_with_the_current_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let previous_key =
            SensitiveResponseKey::derive_from_secret(b"previous-response-encryption-secret");
        let previous_codec = SensitiveResponseCodec::new(previous_key.clone());
        let current_key =
            SensitiveResponseKey::derive_from_secret(b"current-response-encryption-secret");
        let rotating_codec =
            SensitiveResponseCodec::with_previous_key(current_key.clone(), Some(previous_key));
        let current_codec = SensitiveResponseCodec::new(current_key);
        let workspace_id = WorkspaceId::new();

        let old_envelope = previous_codec.encrypt(workspace_id, SCOPE, KEY, &result())?;
        assert_eq!(
            rotating_codec.decrypt::<SensitiveResult>(workspace_id, SCOPE, KEY, old_envelope)?,
            result()
        );
        let new_envelope = rotating_codec.encrypt(workspace_id, SCOPE, KEY, &result())?;
        assert_eq!(
            current_codec.decrypt::<SensitiveResult>(workspace_id, SCOPE, KEY, new_envelope)?,
            result()
        );
        Ok(())
    }
}
