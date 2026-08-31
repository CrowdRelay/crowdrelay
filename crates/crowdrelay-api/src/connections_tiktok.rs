//! TikTok OAuth connection flow for fanbase growth metric sync.
//!
//! Two endpoints:
//!   1. GET /v1/public/connections/tiktok/authorize — public, redirects
//!      to TikTok's OAuth consent page. No admin auth needed — this only
//!      generates a redirect URL and sets a CSRF state cookie.
//!   2. GET /v1/public/connections/tiktok/callback — public (browser
//!      redirect from TikTok), exchanges the authorization code for
//!      access/refresh tokens and stores them encrypted in
//!      fanbase_connections.
//!
//! Token storage: the OAuth tokens (access_token, refresh_token) are
//! encrypted with `SensitiveResponseKey` and stored in
//! `encrypted_access_token` / `encrypted_refresh_token`. The
//! `credential_ref` column stores a short reference identifier
//! (`tiktok:{open_id}`), not a secret blob. The growth metric sync worker
//! decrypts the tokens at point of use and refreshes them when expired.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{Problem, request_id};

/// Cookie name for the OAuth state parameter (CSRF protection).
const STATE_COOKIE: &str = "tiktok_oauth_state";
/// Cookie max age — 10 minutes is enough for the OAuth dance.
const STATE_COOKIE_MAX_AGE: &str = "Max-Age=600";
/// Cookie flags: HttpOnly, SameSite=Lax (needed for the redirect from TikTok).
const STATE_COOKIE_FLAGS: &str = "HttpOnly; SameSite=Lax; Path=/";

/// Scopes requested from TikTok: basic profile + stats (follower count).
const TIKTOK_SCOPES: &str = "user.info.basic,user.info.stats";

/// Allowlist of permitted post-redirect paths. The OAuth callback must
/// not redirect to arbitrary URLs from the state cookie — only these
/// internal control-plane paths are accepted.
const ALLOWED_POST_REDIRECTS: &[&str] = &["/connections", "/connections/tiktok", "/portfolio", "/"];

/// Validates that a post-redirect path is in the allowlist. Returns the
/// validated path or the default `/connections`.
fn validate_post_redirect(path: &str) -> &str {
    if ALLOWED_POST_REDIRECTS.contains(&path) {
        path
    } else {
        "/portfolio"
    }
}

/// Query parameters for the OAuth authorize redirect.
#[derive(Deserialize)]
pub struct AuthorizeParams {
    /// Where to redirect after successful connection. Must be in the
    /// allowlist. Defaults to `/connections`.
    redirect: Option<String>,
}

/// Redirects the operator to TikTok's OAuth consent page. Admin-authenticated.
pub async fn authorize(
    State(_state): State<crate::AppState>,
    Query(params): Query<AuthorizeParams>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let client_key = match std::env::var("CROWDRELAY_TIKTOK_CLIENT_KEY") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Problem::service_unavailable(request_id_value).into_response(),
    };
    let redirect_uri = build_redirect_uri();
    let state = uuid::Uuid::new_v4().to_string();

    // Store the post-connection redirect target in the state cookie so the
    // callback knows where to send the browser after success. The path is
    // validated against an allowlist to prevent open redirect.
    let post_redirect = params
        .redirect
        .as_deref()
        .map(validate_post_redirect)
        .unwrap_or("/");
    let state_value = format!("{state}:{post_redirect}");

    let tiktok_url = format!(
        "https://www.tiktok.com/v2/auth/authorize/?client_key={client_key}\
         &scope={TIKTOK_SCOPES}&response_type=code\
         &redirect_uri={redirect_uri}&state={state}"
    );

    let cookie =
        format!("{STATE_COOKIE}={state_value}; {STATE_COOKIE_MAX_AGE}; {STATE_COOKIE_FLAGS}");

    (
        StatusCode::FOUND,
        [
            (axum::http::header::LOCATION, tiktok_url),
            (SET_COOKIE, cookie),
        ],
    )
        .into_response()
}

/// Query parameters received from TikTok's OAuth redirect.
#[derive(Deserialize)]
pub struct CallbackParams {
    code: String,
    state: String,
}

/// Handles the OAuth callback from TikTok. Public (no admin auth — the
/// browser is redirected here by TikTok). Verifies the state cookie,
/// exchanges the code for tokens, encrypts them, and stores them in
/// fanbase_connections.
pub async fn callback(
    State(state): State<crate::AppState>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);

    // Verify state cookie for CSRF protection.
    let cookie_value = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .find_map(|c| c.trim().strip_prefix(&format!("{STATE_COOKIE}=")))
        });

    let Some(stored_state) = cookie_value else {
        return Problem::bad_request(request_id_value).into_response();
    };

    // The stored state is "uuid:post_redirect_path".
    let (state_uuid, post_redirect) = stored_state.split_once(':').unwrap_or((stored_state, "/"));

    if state_uuid != params.state {
        return Problem::bad_request(request_id_value).into_response();
    }

    let client_key = match std::env::var("CROWDRELAY_TIKTOK_CLIENT_KEY") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Problem::service_unavailable(request_id_value).into_response(),
    };
    let client_secret = match std::env::var("CROWDRELAY_TIKTOK_CLIENT_SECRET") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Problem::service_unavailable(request_id_value).into_response(),
    };
    let redirect_uri = build_redirect_uri();

    // Exchange the authorization code for access + refresh tokens.
    let token_response = state
        .http_client
        .post("https://open.tiktokapis.com/v2/oauth/token/")
        .form(&[
            ("client_key", client_key.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", params.code.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .await;

    let response = match token_response {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "TikTok token exchange HTTP request failed");
            return Problem::service_unavailable(request_id_value).into_response();
        }
    };

    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "TikTok token exchange failed"
        );
        return Problem::service_unavailable(request_id_value).into_response();
    }

    let token_data: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "TikTok token exchange response JSON parse failed");
            return Problem::service_unavailable(request_id_value).into_response();
        }
    };

    // TikTok's token endpoint returns fields at the root level (not wrapped
    // in a "data" object, unlike the user info endpoint).
    let data = &token_data;

    let access_token = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let refresh_token = data
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let open_id = data.get("open_id").and_then(|v| v.as_str()).unwrap_or("");
    let expires_in = data
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(86400);

    if access_token.is_empty() || open_id.is_empty() {
        tracing::warn!(response = %token_data, "TikTok token response has empty access_token or open_id");
        return Problem::service_unavailable(request_id_value).into_response();
    }

    // Compute the access token expiry timestamp.
    let expires_at =
        time::OffsetDateTime::now_utc() + time::Duration::seconds(expires_in.saturating_sub(60));

    let workspace_id: uuid::Uuid = state.ops.workspace_id().into_uuid();

    // Store encrypted tokens via the fanbase repository.
    let repo = crowdrelay_infra::fanbase::PostgresFanbaseRepository::new(state.database.clone())
        .with_encryption_key(state.response_encryption_key.clone());
    if let Err(error) = repo
        .upsert_tiktok_connection(
            workspace_id,
            open_id,
            access_token,
            refresh_token,
            expires_at,
            TIKTOK_SCOPES,
        )
        .await
    {
        tracing::error!(error = %error, "failed to store TikTok connection");
        return Problem::service_unavailable(request_id_value).into_response();
    }

    tracing::info!(open_id = open_id, "TikTok connection established");

    // Clear the state cookie and redirect to the control plane.
    // The post-redirect path was validated against an allowlist when the
    // authorize endpoint set the cookie, but we validate again here for
    // defense in depth.
    let validated_redirect = validate_post_redirect(post_redirect);
    let clear_cookie = format!("{STATE_COOKIE}=; Max-Age=0; {STATE_COOKIE_FLAGS}");
    let redirect_url = format!("https://control.virya.music{validated_redirect}");

    (
        StatusCode::FOUND,
        [
            (axum::http::header::LOCATION, redirect_url),
            (SET_COOKIE, clear_cookie),
        ],
    )
        .into_response()
}

/// Builds the redirect URI for TikTok OAuth. Uses the public API domain.
fn build_redirect_uri() -> String {
    "https://signal-api.virya.music/v1/public/connections/tiktok/callback".to_string()
}
