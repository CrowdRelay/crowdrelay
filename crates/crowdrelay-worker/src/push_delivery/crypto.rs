use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{
    aead, agreement, hkdf,
    rand::SystemRandom,
    signature::{self, EcdsaKeyPair, RsaKeyPair},
};
use serde::Serialize;
use time::OffsetDateTime;

const WEB_PUSH_RECORD_SIZE: u32 = 4096;
const MAX_WEB_PUSH_PLAINTEXT: usize = 3000;

#[derive(Debug)]
pub struct WebPushEnvelope {
    pub body: Vec<u8>,
    pub authorization: String,
}

#[derive(Clone, Copy)]
struct OutputLength(usize);

impl hkdf::KeyType for OutputLength {
    fn len(&self) -> usize {
        self.0
    }
}

pub fn rsa_jwt(private_key_pem: &str, claims: &impl Serialize) -> Result<String> {
    let header = serde_json::json!({"alg":"RS256","typ":"JWT"});
    let signing_input = jwt_signing_input(&header, claims)?;
    let der = decode_private_key_pem(private_key_pem)?;
    let key_pair =
        RsaKeyPair::from_pkcs8(&der).map_err(|_| anyhow!("invalid FCM PKCS#8 RSA private key"))?;
    let rng = SystemRandom::new();
    let mut signature = vec![0_u8; key_pair.public().modulus_len()];
    key_pair
        .sign(
            &signature::RSA_PKCS1_SHA256,
            &rng,
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| anyhow!("FCM service-account JWT signing failed"))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

pub fn vapid_jwt(
    private_key_b64: &str,
    public_key_b64: &str,
    audience: &str,
    subject: &str,
) -> Result<String> {
    let private_key = decode_urlsafe(private_key_b64).context("invalid VAPID private key")?;
    let public_key = decode_urlsafe(public_key_b64).context("invalid VAPID public key")?;
    ensure!(
        private_key.len() == 32,
        "VAPID private key must be 32 bytes"
    );
    ensure!(
        public_key.len() == 65 && public_key.first() == Some(&4),
        "VAPID public key must be an uncompressed P-256 point"
    );
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let claims = serde_json::json!({
        "aud": audience,
        "exp": now.saturating_add(12 * 60 * 60),
        "sub": subject,
    });
    let header = serde_json::json!({"alg":"ES256","typ":"JWT"});
    let signing_input = jwt_signing_input(&header, &claims)?;
    let rng = SystemRandom::new();
    let key_pair = EcdsaKeyPair::from_private_key_and_public_key(
        &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &private_key,
        &public_key,
        &rng,
    )
    .map_err(|_| anyhow!("invalid VAPID P-256 key pair"))?;
    let signature = key_pair
        .sign(&rng, signing_input.as_bytes())
        .map_err(|_| anyhow!("VAPID JWT signing failed"))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_ref())
    ))
}

pub fn web_push_envelope(
    payload: &[u8],
    user_public_key_b64: &str,
    auth_secret_b64: &str,
    vapid_private_key_b64: &str,
    vapid_public_key_b64: &str,
    audience: &str,
    subject: &str,
) -> Result<WebPushEnvelope> {
    ensure!(
        payload.len() <= MAX_WEB_PUSH_PLAINTEXT,
        "Web Push payload exceeds bounded size"
    );
    let user_public_key = decode_urlsafe(user_public_key_b64).context("invalid Web Push p256dh")?;
    let auth_secret = decode_urlsafe(auth_secret_b64).context("invalid Web Push auth secret")?;
    ensure!(
        user_public_key.len() == 65 && user_public_key.first() == Some(&4),
        "Web Push p256dh must be an uncompressed P-256 point"
    );
    ensure!(auth_secret.len() >= 16, "Web Push auth secret is too short");

    let rng = SystemRandom::new();
    let ephemeral = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| anyhow!("Web Push ephemeral key generation failed"))?;
    let server_public = ephemeral
        .compute_public_key()
        .map_err(|_| anyhow!("Web Push ephemeral public key derivation failed"))?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &user_public_key);
    let shared_secret = agreement::agree_ephemeral(ephemeral, &peer, |material| material.to_vec())
        .map_err(|_| anyhow!("Web Push ECDH failed"))?;

    let auth_prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &auth_secret).extract(&shared_secret);
    let key_info = [
        b"WebPush: info\0".as_slice(),
        user_public_key.as_slice(),
        server_public.as_ref(),
    ];
    let ikm = hkdf_expand(&auth_prk, &key_info, 32).context("Web Push auth HKDF failed")?;

    let mut salt = [0_u8; 16];
    ring::rand::SecureRandom::fill(&rng, &mut salt)
        .map_err(|_| anyhow!("Web Push salt generation failed"))?;
    let content_prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(&ikm);
    let cek = hkdf_expand(&content_prk, &[b"Content-Encoding: aes128gcm\0"], 16)
        .context("Web Push CEK derivation failed")?;
    let nonce_bytes = hkdf_expand(&content_prk, &[b"Content-Encoding: nonce\0"], 12)
        .context("Web Push nonce derivation failed")?;
    let nonce_array: [u8; 12] = nonce_bytes
        .try_into()
        .map_err(|_| anyhow!("invalid Web Push nonce length"))?;

    let unbound = aead::UnboundKey::new(&aead::AES_128_GCM, &cek)
        .map_err(|_| anyhow!("Web Push AES key setup failed"))?;
    let key = aead::LessSafeKey::new(unbound);
    let mut ciphertext = Vec::with_capacity(payload.len().saturating_add(17));
    ciphertext.extend_from_slice(payload);
    ciphertext.push(2);
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce_array),
        aead::Aad::empty(),
        &mut ciphertext,
    )
    .map_err(|_| anyhow!("Web Push encryption failed"))?;
    ensure!(
        ciphertext.len() <= WEB_PUSH_RECORD_SIZE as usize,
        "Web Push record exceeds bounded size"
    );

    let server_public = server_public.as_ref();
    let key_id_len =
        u8::try_from(server_public.len()).context("Web Push ephemeral key too large")?;
    let mut body = Vec::with_capacity(16 + 4 + 1 + server_public.len() + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&WEB_PUSH_RECORD_SIZE.to_be_bytes());
    body.push(key_id_len);
    body.extend_from_slice(server_public);
    body.extend_from_slice(&ciphertext);

    let vapid_token = vapid_jwt(
        vapid_private_key_b64,
        vapid_public_key_b64,
        audience,
        subject,
    )?;
    let authorization = format!("vapid t={vapid_token}, k={}", vapid_public_key_b64.trim());
    Ok(WebPushEnvelope {
        body,
        authorization,
    })
}

fn jwt_signing_input(header: &impl Serialize, claims: &impl Serialize) -> Result<String> {
    let header = serde_json::to_vec(header).context("JWT header serialization failed")?;
    let claims = serde_json::to_vec(claims).context("JWT claims serialization failed")?;
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    ))
}

fn decode_private_key_pem(value: &str) -> Result<Vec<u8>> {
    let mut in_key = false;
    let mut encoded = String::new();
    for line in value.lines().map(str::trim) {
        if line == "-----BEGIN PRIVATE KEY-----" {
            in_key = true;
            continue;
        }
        if line == "-----END PRIVATE KEY-----" {
            break;
        }
        if in_key && !line.is_empty() {
            encoded.push_str(line);
        }
    }
    if encoded.is_empty() {
        bail!("private key PEM must contain an unencrypted PKCS#8 PRIVATE KEY block");
    }
    STANDARD
        .decode(encoded)
        .context("private key PEM base64 is invalid")
}

fn decode_urlsafe(value: &str) -> Result<Vec<u8>> {
    let normalized = value.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(normalized)
        .map_err(|error| anyhow!("invalid URL-safe base64: {error}"))
}

fn hkdf_expand(prk: &hkdf::Prk, info: &[&[u8]], len: usize) -> Result<Vec<u8>> {
    let okm = prk
        .expand(info, OutputLength(len))
        .map_err(|_| anyhow!("HKDF expand rejected output length"))?;
    let mut output = vec![0_u8; len];
    okm.fill(&mut output)
        .map_err(|_| anyhow!("HKDF output fill failed"))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlsafe_decoder_accepts_padded_and_unpadded_values() {
        let original = [1_u8, 2, 3, 4, 5];
        let encoded = URL_SAFE_NO_PAD.encode(original);
        assert_eq!(
            decode_urlsafe(&encoded).ok().as_deref(),
            Some(original.as_slice())
        );
        let padded = format!("{encoded}=");
        assert_eq!(
            decode_urlsafe(&padded).ok().as_deref(),
            Some(original.as_slice())
        );
    }
}
