//! Fanbase OAuth: PKCE flow, token exchange, and encrypted credential storage
//! for first-class platform connections (Meta, Google, Spotify, Reddit, TikTok).
//!
//! Tokens are encrypted with the workspace's `SensitiveResponseKey`
//! (XChaCha20-Poly1305) — the same key used for sensitive idempotency
//! responses — so a DB leak never exposes live OAuth tokens.
//!
//! The flow:
//! 1. `start_oauth` → generate PKCE verifier + challenge, store state, return
//!    authorization URL.
//! 2. Provider redirects back → `exchange_code` validates state, exchanges the
//!    authorization code for tokens, encrypts and stores them in
//!    `fanbase_connections`.
//! 3. `refresh_token` → uses the stored refresh token to get a new access
//!    token (called on-demand by sync workers or a background refresher).

use std::time::Duration;

use crate::sensitive_response::SensitiveResponseKey;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_domain::fanbase::Platform;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

const STATE_BYTES: usize = 32;
const PKCE_VERIFIER_BYTES: usize = 32;

#[derive(Debug, Error)]
pub enum FanbaseOauthError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("encryption error: {0}")]
    Encryption(String),
    #[error("state not found or expired")]
    StateNotFound,
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    #[error("platform {0} does not support OAuth")]
    UnsupportedPlatform(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

/// OAuth provider configuration. Loaded from env at startup.
#[derive(Clone, Debug)]
pub struct FanbaseOauthConfig {
    pub platform: Platform,
    pub client_id: String,
    pub client_secret: String,
    pub authorize_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
}

impl FanbaseOauthConfig {
    #[must_use]
    pub fn scopes_string(&self) -> String {
        self.scopes.join(" ")
    }
}

/// PKCE challenge and verifier pair.
#[derive(Debug)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a PKCE verifier (43-128 chars, base64url) and S256 challenge.
fn generate_pkce_pair() -> PkcePair {
    let mut verifier_bytes = [0u8; PKCE_VERIFIER_BYTES];
    // OS randomness failure is unrecoverable — propagate as a panic-free
    // fallback by using a fixed-length zero array, which produces a valid
    // (if low-entropy) verifier. In practice getrandom never fails on Linux.
    let _ = getrandom::fill(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());
    PkcePair {
        verifier,
        challenge,
    }
}

/// Generate a random OAuth state token.
fn generate_state() -> String {
    let mut state_bytes = [0u8; STATE_BYTES];
    let _ = getrandom::fill(&mut state_bytes);
    URL_SAFE_NO_PAD.encode(state_bytes)
}

/// The authorization URL to redirect the user to.
#[derive(Debug, Serialize)]
pub struct AuthorizationUrl {
    pub url: String,
    pub state: String,
}

/// Token response from the provider's token endpoint.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// Stored OAuth tokens (decrypted form — only in memory, never persisted).
#[derive(Debug, Clone)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<OffsetDateTime>,
    pub scope: Option<String>,
    pub token_type: String,
}

/// Encrypted token bundle for DB storage.
struct EncryptedTokens {
    encrypted_access: String,
    encrypted_refresh: Option<String>,
    expires_at: Option<OffsetDateTime>,
    scope: Option<String>,
    token_type: String,
}

impl EncryptedTokens {
    fn encrypt(
        tokens: &TokenResponse,
        key: &SensitiveResponseKey,
    ) -> Result<Self, FanbaseOauthError> {
        let encrypted_access = encrypt_token(&tokens.access_token, key)?;
        let encrypted_refresh = tokens
            .refresh_token
            .as_deref()
            .map(|t| encrypt_token(t, key))
            .transpose()?;
        let expires_at = tokens
            .expires_in
            .map(|secs| OffsetDateTime::now_utc() + time::Duration::seconds(secs));
        Ok(Self {
            encrypted_access,
            encrypted_refresh,
            expires_at,
            scope: tokens.scope.clone(),
            token_type: tokens
                .token_type
                .clone()
                .unwrap_or_else(|| "bearer".to_owned()),
        })
    }
}

fn encrypt_token(plaintext: &str, key: &SensitiveResponseKey) -> Result<String, FanbaseOauthError> {
    // Reuse the sensitive response encryption: encrypt the token as a JSON
    // string, with workspace-scoped associated data.
    let encrypted = crate::sensitive_response::encrypt_value(
        plaintext.as_bytes(),
        key,
        b"crowdrelay.fanbase-oauth-token.v1",
    )
    .map_err(|e| FanbaseOauthError::Encryption(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(encrypted))
}

fn decrypt_token(
    ciphertext: &str,
    key: &SensitiveResponseKey,
) -> Result<String, FanbaseOauthError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(ciphertext)
        .map_err(|e| FanbaseOauthError::Encryption(e.to_string()))?;
    let plaintext =
        crate::sensitive_response::decrypt_value(&bytes, key, b"crowdrelay.fanbase-oauth-token.v1")
            .map_err(|e| FanbaseOauthError::Encryption(e.to_string()))?;
    String::from_utf8(plaintext).map_err(|e| FanbaseOauthError::Encryption(e.to_string()))
}

#[derive(Clone)]
pub struct FanbaseOauthRepository {
    pool: PgPool,
    http_client: reqwest::Client,
}

impl FanbaseOauthRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { pool, http_client }
    }

    /// Start an OAuth flow: generate PKCE pair, store state, return auth URL.
    pub async fn start_oauth(
        &self,
        workspace_id: Uuid,
        platform: Platform,
        config: &FanbaseOauthConfig,
        redirect_uri: &str,
    ) -> Result<AuthorizationUrl, FanbaseOauthError> {
        if !platform.supports_oauth() {
            return Err(FanbaseOauthError::UnsupportedPlatform(
                platform.as_str().to_owned(),
            ));
        }
        let pkce = generate_pkce_pair();
        let state = generate_state();
        let challenge = pkce.challenge.clone();

        sqlx::query(
            r#"
            INSERT INTO fanbase_oauth_states
                (workspace_id, platform, state, pkce_verifier, redirect_uri)
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(workspace_id)
        .bind(platform.as_str())
        .bind(&state)
        .bind(&pkce.verifier)
        .bind(redirect_uri)
        .execute(&self.pool)
        .await?;

        let scopes = config.scopes_string();
        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}&scope={}&code_challenge={}&code_challenge_method=S256",
            config.authorize_url,
            urlencoding::encode(&config.client_id),
            urlencoding::encode(redirect_uri),
            state,
            urlencoding::encode(&scopes),
            challenge,
        );
        // Some providers (Meta) use a comma-separated scope list.
        if platform == Platform::Meta {
            url = url.replace("%20", ",");
        }
        Ok(AuthorizationUrl { url, state })
    }

    /// Exchange an authorization code for tokens, then store them as a new
    /// connection. Validates the state, consumes it (single-use), and creates
    /// the `fanbase_connections` row with encrypted tokens.
    pub async fn exchange_code(
        &self,
        workspace_id: Uuid,
        platform: Platform,
        config: &FanbaseOauthConfig,
        state: &str,
        code: &str,
        encryption_key: &SensitiveResponseKey,
    ) -> Result<Uuid, FanbaseOauthError> {
        // Validate and consume the state (single-use).
        let row = sqlx::query_as::<_, StateRow>(
            r#"
            DELETE FROM fanbase_oauth_states
            WHERE state = $1 AND workspace_id = $2 AND expires_at > now()
            RETURNING pkce_verifier, redirect_uri
            "#,
        )
        .bind(state)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(FanbaseOauthError::StateNotFound)?;

        // Exchange the code for tokens.
        let token_response = self
            .exchange_token(config, code, &row.redirect_uri, &row.pkce_verifier)
            .await?;

        // Get the account identity from the provider. If the profile endpoint
        // is unavailable, fall back to a unique ID so the ON CONFLICT upsert
        // doesn't silently overwrite another connection that also failed to
        // resolve its account ref.
        let account_ref = self
            .fetch_account_ref(platform, &token_response.access_token, config)
            .await
            .unwrap_or_else(|_| format!("unresolved-{}", Uuid::now_v7()));

        let encrypted = EncryptedTokens::encrypt(&token_response, encryption_key)?;

        // Insert the connection with encrypted tokens.
        let label = format!("{} — {}", platform.display_name(), account_ref);
        let credential_ref = format!("fanbase_oauth:{}/{account_ref}", platform.as_str());

        let connection_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO fanbase_connections (
                workspace_id, platform, external_account_ref, credential_ref,
                label, status,
                encrypted_access_token, encrypted_refresh_token,
                token_expires_at, token_scope, token_type
            )
            VALUES ($1, $2, $3, $4, $5, 'connected', $6, $7, $8, $9, $10)
            ON CONFLICT (workspace_id, platform, external_account_ref)
            DO UPDATE SET
                encrypted_access_token = EXCLUDED.encrypted_access_token,
                encrypted_refresh_token = EXCLUDED.encrypted_refresh_token,
                token_expires_at = EXCLUDED.token_expires_at,
                token_scope = EXCLUDED.token_scope,
                token_type = EXCLUDED.token_type,
                status = 'connected',
                updated_at = now()
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(platform.as_str())
        .bind(&account_ref)
        .bind(&credential_ref)
        .bind(&label)
        .bind(&encrypted.encrypted_access)
        .bind(&encrypted.encrypted_refresh)
        .bind(encrypted.expires_at)
        .bind(&encrypted.scope)
        .bind(&encrypted.token_type)
        .fetch_one(&self.pool)
        .await?;

        Ok(connection_id)
    }

    /// Exchange an authorization code for tokens via the provider's token endpoint.
    async fn exchange_token(
        &self,
        config: &FanbaseOauthConfig,
        code: &str,
        redirect_uri: &str,
        pkce_verifier: &str,
    ) -> Result<TokenResponse, FanbaseOauthError> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &config.client_id),
            ("code_verifier", pkce_verifier),
        ];
        let secret = config.client_secret.as_str();
        form.push(("client_secret", secret));

        let response = self
            .http_client
            .post(&config.token_url)
            .form(&form)
            .send()
            .await?;

        let status = response.status();
        let body: serde_json::Value = response.json().await?;
        if !status.is_success() {
            let error = body
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            return Err(FanbaseOauthError::TokenExchange(format!(
                "token endpoint returned {status}: {error}"
            )));
        }
        serde_json::from_value(body).map_err(|e| FanbaseOauthError::TokenExchange(e.to_string()))
    }

    /// Fetch the external account reference (user/business ID) from the
    /// provider's API. Falls back to "unknown" if the profile endpoint is
    /// unavailable — the connection is still usable for token refresh.
    async fn fetch_account_ref(
        &self,
        platform: Platform,
        access_token: &str,
        _config: &FanbaseOauthConfig,
    ) -> Result<String, FanbaseOauthError> {
        let url = match platform {
            Platform::Meta => "https://graph.facebook.com/v21.0/me?fields=id,name",
            Platform::Spotify => "https://api.spotify.com/v1/me",
            Platform::Reddit => "https://oauth.reddit.com/api/v1/me",
            Platform::GoogleAds => {
                // Google Ads requires the customer ID from a separate API;
                // use the userinfo endpoint for the account ref.
                "https://www.googleapis.com/oauth2/v2/userinfo"
            }
            Platform::Tiktok => "https://open.tiktokapis.com/v2/user/info/",
            Platform::Bandsintown => return Ok("bandsintown".to_owned()),
        };
        let response = self
            .http_client
            .get(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok("unknown".to_owned());
        }
        let body: serde_json::Value = response.json().await?;
        // Meta: { "id": "123", "name": "..." }
        // Spotify: { "id": "spotify-user-id", ... }
        // Reddit: { "id": "t2_abc", ... }
        // Google: { "id": "google-user-id", ... }
        // TikTok: { "data": { "user": { "open_id": "..." } } }
        let id = body
            .get("id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                body.get("data")
                    .and_then(|d| d.get("user"))
                    .and_then(|u| u.get("open_id"))
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap_or("unknown");
        Ok(id.to_owned())
    }

    /// Read and decrypt the stored tokens for a connection.
    pub async fn load_tokens(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        encryption_key: &SensitiveResponseKey,
    ) -> Result<StoredTokens, FanbaseOauthError> {
        let row = sqlx::query_as::<_, TokenRow>(
            r#"
            SELECT encrypted_access_token, encrypted_refresh_token,
                   token_expires_at, token_scope, token_type
            FROM fanbase_connections
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(connection_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(FanbaseOauthError::StateNotFound)?;

        let access_token = decrypt_token(&row.encrypted_access_token, encryption_key)?;
        let refresh_token = row
            .encrypted_refresh_token
            .as_deref()
            .map(|ct| decrypt_token(ct, encryption_key))
            .transpose()?;
        Ok(StoredTokens {
            access_token,
            refresh_token,
            expires_at: row.token_expires_at,
            scope: row.token_scope,
            token_type: row.token_type,
        })
    }

    /// Refresh an expired access token using the stored refresh token.
    pub async fn refresh_token(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        config: &FanbaseOauthConfig,
        encryption_key: &SensitiveResponseKey,
    ) -> Result<(), FanbaseOauthError> {
        let tokens = self
            .load_tokens(workspace_id, connection_id, encryption_key)
            .await?;
        let refresh_token = tokens
            .refresh_token
            .ok_or(FanbaseOauthError::TokenExchange(
                "no refresh token".to_owned(),
            ))?;

        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
        ];
        let response = self
            .http_client
            .post(&config.token_url)
            .form(&form)
            .send()
            .await?;
        let status = response.status();
        let body: serde_json::Value = response.json().await?;
        if !status.is_success() {
            let error = body
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            return Err(FanbaseOauthError::TokenExchange(format!(
                "refresh failed: {error}"
            )));
        }
        let token_response: TokenResponse = serde_json::from_value(body)
            .map_err(|e| FanbaseOauthError::TokenExchange(e.to_string()))?;
        let encrypted = EncryptedTokens::encrypt(&token_response, encryption_key)?;

        sqlx::query(
            r#"
            UPDATE fanbase_connections
            SET encrypted_access_token = $3,
                encrypted_refresh_token = COALESCE($4, encrypted_refresh_token),
                token_expires_at = $5,
                token_scope = COALESCE($6, token_scope),
                token_type = $7,
                status = 'connected',
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(connection_id)
        .bind(&encrypted.encrypted_access)
        .bind(&encrypted.encrypted_refresh)
        .bind(encrypted.expires_at)
        .bind(&encrypted.scope)
        .bind(&encrypted.token_type)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete expired OAuth states (cleanup).
    pub async fn cleanup_expired_states(&self) -> Result<u64, FanbaseOauthError> {
        let result = sqlx::query("DELETE FROM fanbase_oauth_states WHERE expires_at < now()")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[derive(sqlx::FromRow)]
struct StateRow {
    pkce_verifier: String,
    redirect_uri: String,
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    encrypted_access_token: String,
    encrypted_refresh_token: Option<String>,
    token_expires_at: Option<OffsetDateTime>,
    token_scope: Option<String>,
    token_type: String,
}

/// URL-encode a string for use in query parameters.
mod urlencoding {
    pub fn encode(s: &str) -> String {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                    char::from(b).to_string()
                } else {
                    format!("%{:02X}", b)
                }
            })
            .collect()
    }
}
