//! Fanbase credential storage: encrypted token storage for platform
//! connections and Reddit "script app" password grant authentication.
//!
//! Tokens are encrypted with the workspace's `SensitiveResponseKey`
//! (XChaCha20-Poly1305) — the same key used for sensitive idempotency
//! responses — so a DB leak never exposes live tokens.
//!
//! Reddit script apps authenticate with username/password (password grant)
//! instead of the web-app OAuth redirect flow. The access token is stored
//! in `fanbase_connections` so the existing `load_tokens` path works.

use std::time::Duration;

use crate::sensitive_response::SensitiveResponseKey;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crowdrelay_domain::fanbase::Platform;
use serde::Deserialize;
use sqlx::PgPool;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

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
    #[error("provider profile fetch failed")]
    ProfileFetchFailed,
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

    /// Exchange username/password for an access token using the OAuth2
    /// "password" grant (Reddit "script" app type). Unlike the web-app flow,
    /// this doesn't require a redirect URI or user interaction — the worker
    /// authenticates directly with the Reddit account credentials.
    ///
    /// Reddit script apps do NOT issue a refresh token. When the access token
    /// expires, the caller must call this method again. The token is stored
    /// in `fanbase_connections` so the existing `load_tokens` path works.
    ///
    /// If a Reddit connection already exists for the workspace, it is updated.
    /// Otherwise a new connection row is created.
    pub async fn password_grant(
        &self,
        workspace_id: Uuid,
        config: &FanbaseOauthConfig,
        username: &str,
        password: &str,
        encryption_key: &SensitiveResponseKey,
    ) -> Result<StoredTokens, FanbaseOauthError> {
        let form = [
            ("grant_type", "password"),
            ("username", username),
            ("password", password),
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
                "password grant failed: {error}"
            )));
        }
        let token_response: TokenResponse = serde_json::from_value(body)
            .map_err(|e| FanbaseOauthError::TokenExchange(e.to_string()))?;
        let encrypted = EncryptedTokens::encrypt(&token_response, encryption_key)?;

        // Upsert the connection: if a Reddit connection exists for this
        // username, update it; otherwise create one. The
        // `external_account_ref` is the Reddit username — there's no
        // separate "me" API call to resolve it for script apps.
        // `credential_ref` and `label` are NOT NULL with CHECK constraints;
        // for script-app connections there's no external credential ref, so
        // we use a synthetic placeholder.
        let connection_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO fanbase_connections (
                id, workspace_id, platform, external_account_ref,
                credential_ref, label, status,
                encrypted_access_token, encrypted_refresh_token,
                token_expires_at, token_scope, token_type
            )
            VALUES ($1, $2, 'reddit', $3, 'script-app', $3, 'connected', $4, $5, $6, $7, $8)
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
        .bind(connection_id)
        .bind(workspace_id)
        .bind(username)
        .bind(&encrypted.encrypted_access)
        .bind(&encrypted.encrypted_refresh)
        .bind(encrypted.expires_at)
        .bind(&encrypted.scope)
        .bind(&encrypted.token_type)
        .fetch_one(&self.pool)
        .await?;

        Ok(StoredTokens {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            expires_at: encrypted.expires_at,
            scope: token_response.scope,
            token_type: token_response
                .token_type
                .unwrap_or_else(|| "bearer".to_owned()),
        })
    }
}

/// Registers a manually-posted Reddit URL for a community post that was
/// in `awaiting_manual_post` status. Extracts the Reddit post ID from
/// the URL and transitions the row to `posted` so the metrics poller
/// can track it. Called by the API layer when the operator confirms
/// they've posted manually on Reddit.
///
/// # Errors
/// Returns `FanbaseOauthError::Database` if the post doesn't exist or
/// isn't in `awaiting_manual_post` status.
pub async fn register_manual_reddit_post(
    pool: &PgPool,
    workspace_id: Uuid,
    community_post_id: Uuid,
    reddit_post_url: &str,
) -> Result<(), FanbaseOauthError> {
    // Extract the Reddit post ID from the URL.
    // Reddit post URLs look like: https://www.reddit.com/r/subreddit/comments/abc123/title/
    // The post ID is the 6-character alphanumeric segment after "comments/".
    let reddit_post_id = extract_reddit_post_id(reddit_post_url).ok_or_else(|| {
        FanbaseOauthError::TokenExchange(format!(
            "could not extract post ID from URL: {reddit_post_url}"
        ))
    })?;

    let result = sqlx::query(
        r#"
        UPDATE community_posts
        SET status = 'posted',
            reddit_post_id = $3,
            reddit_post_url = $4,
            posted_at = now(),
            updated_at = now(),
            error_message = NULL
        WHERE id = $1
          AND workspace_id = $2
          AND status = 'awaiting_manual_post'
        "#,
    )
    .bind(community_post_id)
    .bind(workspace_id)
    .bind(&reddit_post_id)
    .bind(reddit_post_url)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(FanbaseOauthError::TokenExchange(
            "community post not found or not in awaiting_manual_post status".to_owned(),
        ));
    }
    Ok(())
}

/// Extracts the Reddit post ID from a URL like:
/// `https://www.reddit.com/r/metal/comments/abc123/title/` → `abc123`
fn extract_reddit_post_id(url: &str) -> Option<String> {
    let parts: Vec<&str> = url.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "comments"
            && let Some(id) = parts.get(i + 1)
            && !id.is_empty()
        {
            return Some((*id).to_owned());
        }
    }
    None
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    encrypted_access_token: String,
    encrypted_refresh_token: Option<String>,
    token_expires_at: Option<OffsetDateTime>,
    token_scope: Option<String>,
    token_type: String,
}
