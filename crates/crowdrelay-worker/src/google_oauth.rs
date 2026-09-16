//! Shared Google OAuth token resolution for the contacts intake workers
//! (gdrive, gmail). Decrypts the connection's access token at point of
//! use; when it is within a minute of expiry, runs the refresh-token grant
//! against Google's token endpoint, re-encrypts under the same AAD, and
//! persists through the infra repository. Plaintext never leaves this
//! module.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_infra::{
    fanbase::fanbase_token_aad,
    gdrive::PostgresGDriveRepository,
    sensitive_response::{SensitiveResponseKey, decrypt_value, encrypt_value},
};
use uuid::Uuid;

#[derive(Debug, serde::Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    expires_in: i64,
}

/// A valid access token for a Google connection. `platform` ("gdrive" |
/// "gmail") is the AAD component the tokens were encrypted under.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_google_access_token(
    repo: &PostgresGDriveRepository,
    http: &reqwest::Client,
    key: &SensitiveResponseKey,
    workspace_id: Uuid,
    connection_id: Uuid,
    account_ref: &str,
    platform: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<String, String> {
    let (enc_access, enc_refresh, expires_at) = repo
        .connection_tokens(workspace_id, connection_id)
        .await
        .map_err(|e| e.to_string())?;
    let enc_access = enc_access.ok_or("connection missing encrypted_access_token")?;
    let aad = fanbase_token_aad(workspace_id, platform, account_ref);

    let decode = |value: &str, label: &str| -> Result<String, String> {
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| format!("{label} is not valid base64"))?;
        let plain = decrypt_value(&bytes, key, &aad)
            .map_err(|e| format!("{label} decryption failed: {e}"))?;
        String::from_utf8(plain).map_err(|_| format!("{label} is not valid UTF-8"))
    };

    let access_token = decode(&enc_access, "access token")?;
    let fresh = expires_at
        .is_some_and(|at| at > time::OffsetDateTime::now_utc() + time::Duration::seconds(60));
    if fresh {
        return Ok(access_token);
    }

    let refresh_token = decode(
        enc_refresh
            .as_deref()
            .ok_or("connection missing encrypted_refresh_token")?,
        "refresh token",
    )?;
    let (Some(client_id), Some(client_secret)) = (client_id, client_secret) else {
        return Err("google OAuth client not configured".to_string());
    };
    let response = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .send()
        .await
        .map_err(|e| format!("token refresh request failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("token refresh failed status={status} {body}"));
    }
    let token: GoogleTokenResponse = response
        .json()
        .await
        .map_err(|e| format!("token refresh response parse failed: {e}"))?;
    let new_expiry = time::OffsetDateTime::now_utc()
        + time::Duration::seconds(token.expires_in.saturating_sub(60));
    let encrypted = encrypt_value(token.access_token.as_bytes(), key, &aad)
        .map_err(|e| format!("access token encryption failed: {e}"))?;
    repo.store_refreshed_access_token(
        workspace_id,
        connection_id,
        &URL_SAFE_NO_PAD.encode(encrypted),
        new_expiry,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(token.access_token)
}

/// Resolves a connection's account ref then its access token — the
/// point-of-use path every download takes so a rotated token is never
/// stale.
#[allow(clippy::too_many_arguments)]
pub async fn access_token_for_connection(
    repo: &PostgresGDriveRepository,
    http: &reqwest::Client,
    key: &SensitiveResponseKey,
    workspace_id: Uuid,
    connection_id: Uuid,
    platform: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<String, String> {
    let account_ref = repo
        .connection_account_ref(workspace_id, connection_id)
        .await
        .map_err(|e| e.to_string())?;
    resolve_google_access_token(
        repo,
        http,
        key,
        workspace_id,
        connection_id,
        &account_ref,
        platform,
        client_id,
        client_secret,
    )
    .await
}

/// HTTP timeout the callers share for Google API requests.
pub const GOOGLE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
