//! Google Drive OAuth connection flow for contact import.
//!
//! Two endpoints:
//!   1. GET /v1/public/connections/gdrive/authorize — redirects to Google's
//!      OAuth consent page, requesting `drive.readonly` + `openid email`.
//!   2. GET /v1/public/connections/gdrive/callback — exchanges the code for
//!      tokens, resolves the Google user id, encrypts the tokens into
//!      `fanbase_connections` (platform `gdrive`), and wakes the contacts
//!      sync worker via pg_notify.
//!
//! The OAuth client is the existing Google Ads client
//! (`CROWDRELAY_GOOGLE_ADS_CLIENT_ID` / `…_SECRET`) — one Google OAuth
//! client for the whole tenant side, per product decision. The Drive
//! readonly scope may require Google verification for production use; an
//! unverified app works for test users.
//!
//! Token storage matches TikTok: encrypted with `SensitiveResponseKey` under
//! `crowdrelay.fanbase.oauth.gdrive.v1` AAD, decrypted by the gdrive
//! contacts worker at point of use only.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{Problem, request_id};

/// Cookie name for the OAuth state parameter (CSRF protection).
const STATE_COOKIE: &str = "gdrive_oauth_state";
/// Cookie max age — 10 minutes is enough for the OAuth dance.
const STATE_COOKIE_MAX_AGE: &str = "Max-Age=600";
/// Cookie flags: HttpOnly, SameSite=Lax (needed for the redirect from Google).
const STATE_COOKIE_FLAGS: &str = "HttpOnly; Secure; SameSite=Lax; Path=/";

/// Scopes: read-only Drive access + enough identity to name the account.
const GDRIVE_SCOPES: &str = "openid email https://www.googleapis.com/auth/drive.readonly";

/// Allowlist of permitted post-redirect paths. Same rule as TikTok —
/// the callback never redirects to an arbitrary URL from the cookie.
const ALLOWED_POST_REDIRECTS: &[&str] = &["/connections", "/connections/gdrive", "/audience", "/"];

/// Validates a post-redirect path against the allowlist plus the dynamic
/// `/tenants/{slug}/audience` control-plane route.
fn validate_post_redirect(path: &str) -> &str {
    if ALLOWED_POST_REDIRECTS.contains(&path) {
        return path;
    }
    if let Some(rest) = path.strip_prefix("/tenants/")
        && let Some(slug) = rest.strip_suffix("/audience")
        && !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return path;
    }
    "/"
}

/// Query parameters for the OAuth authorize redirect.
#[derive(Deserialize)]
pub struct AuthorizeParams {
    /// Where to redirect after successful connection. Allowlisted.
    redirect: Option<String>,
}

fn google_client_id() -> Option<String> {
    std::env::var("CROWDRELAY_GOOGLE_ADS_CLIENT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn google_client_secret() -> Option<String> {
    std::env::var("CROWDRELAY_GOOGLE_ADS_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Redirects the operator to Google's OAuth consent page.
/// `access_type=offline` + `prompt=consent` guarantee a refresh token on
/// every connect, including reconnects.
pub async fn authorize(
    State(_state): State<crate::AppState>,
    Query(params): Query<AuthorizeParams>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(client_id) = google_client_id() else {
        return Problem::service_unavailable(request_id_value).into_response();
    };
    let redirect_uri = build_redirect_uri();
    let state = uuid::Uuid::new_v4().to_string();

    let post_redirect = params
        .redirect
        .as_deref()
        .map(validate_post_redirect)
        .unwrap_or("/");
    let state_value = format!("{state}:{post_redirect}");

    let google_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={client_id}\
         &redirect_uri={}&response_type=code\
         &scope={}&access_type=offline&prompt=consent&state={state}",
        urlencoding(&redirect_uri),
        urlencoding(GDRIVE_SCOPES),
    );

    let cookie =
        format!("{STATE_COOKIE}={state_value}; {STATE_COOKIE_MAX_AGE}; {STATE_COOKIE_FLAGS}");

    (
        StatusCode::FOUND,
        [
            (axum::http::header::LOCATION, google_url),
            (SET_COOKIE, cookie),
        ],
    )
        .into_response()
}

/// Query parameters received from Google's OAuth redirect.
#[derive(Deserialize)]
pub struct CallbackParams {
    code: String,
    state: String,
}

/// Handles the OAuth callback. Public (browser redirect from Google).
/// Verifies the state cookie, exchanges the code, resolves the Google
/// user id via userinfo, and stores encrypted tokens.
pub async fn callback(
    State(state): State<crate::AppState>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);

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
    let (state_uuid, post_redirect) = stored_state.split_once(':').unwrap_or((stored_state, "/"));
    if state_uuid != params.state {
        return Problem::bad_request(request_id_value).into_response();
    }

    let (Some(client_id), Some(client_secret)) = (google_client_id(), google_client_secret())
    else {
        return Problem::service_unavailable(request_id_value).into_response();
    };
    let redirect_uri = build_redirect_uri();

    let response = match state
        .http_client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", params.code.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Google token exchange HTTP request failed");
            return Problem::service_unavailable(request_id_value).into_response();
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "Google token exchange failed"
        );
        return Problem::service_unavailable(request_id_value).into_response();
    }
    let token_data: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Google token exchange response JSON parse failed");
            return Problem::service_unavailable(request_id_value).into_response();
        }
    };

    let access_token = token_data
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let refresh_token = token_data
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expires_in = token_data
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);

    if access_token.is_empty() || refresh_token.is_empty() {
        // Never log the raw token response — it carries plaintext tokens.
        tracing::warn!(
            has_access_token = !access_token.is_empty(),
            has_refresh_token = !refresh_token.is_empty(),
            "Google token response missing access_token or refresh_token"
        );
        return Problem::service_unavailable(request_id_value).into_response();
    }

    // Resolve the Google account id — the external_account_ref the
    // connection is keyed by and the AAD component tokens encrypt under.
    let userinfo = state
        .http_client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access_token)
        .send()
        .await;
    let google_user_id = match userinfo {
        Ok(r) if r.status().is_success() => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_owned)),
        _ => None,
    };
    let Some(google_user_id) = google_user_id else {
        tracing::warn!("Google userinfo lookup failed after token exchange");
        return Problem::service_unavailable(request_id_value).into_response();
    };

    let expires_at =
        time::OffsetDateTime::now_utc() + time::Duration::seconds(expires_in.saturating_sub(60));
    let workspace_id: uuid::Uuid = state.ops.workspace_id().into_uuid();

    let repo = crowdrelay_infra::fanbase::PostgresFanbaseRepository::new(state.database.clone())
        .with_encryption_key(state.response_encryption_key.clone());
    if let Err(error) = repo
        .upsert_gdrive_connection(
            workspace_id,
            &google_user_id,
            access_token,
            refresh_token,
            expires_at,
            GDRIVE_SCOPES,
            "Google Drive",
        )
        .await
    {
        tracing::error!(error = %error, "failed to store Google Drive connection");
        return Problem::service_unavailable(request_id_value).into_response();
    }

    tracing::info!(google_user_id = %google_user_id, "Google Drive connection established");

    let validated_redirect = validate_post_redirect(post_redirect);
    let clear_cookie = format!("{STATE_COOKIE}=; Max-Age=0; {STATE_COOKIE_FLAGS}");
    let redirect_url = format!("https://control.crowdrelay.music{validated_redirect}");

    (
        StatusCode::FOUND,
        [
            (axum::http::header::LOCATION, redirect_url),
            (SET_COOKIE, clear_cookie),
        ],
    )
        .into_response()
}

/// Minimal percent-encoding for the scope parameter — Google's scope list
/// is space-separated and contains a URL, so it must not travel raw.
fn urlencoding(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace(':', "%3A")
        .replace('/', "%2F")
}

/// Redirect URI for Google OAuth. Must be registered as an authorized
/// redirect URI on the shared Google OAuth client.
fn build_redirect_uri() -> String {
    "https://signal-api.virya.music/v1/public/connections/gdrive/callback".to_string()
}
