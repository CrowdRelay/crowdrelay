//! TikTok OAuth connection flow for fanbase growth metric sync.
//!
//! Two endpoints:
//!   1. GET /v1/admin/connections/tiktok/authorize — admin-initiated,
//!      redirects to TikTok's OAuth consent page.
//!   2. GET /v1/public/connections/tiktok/callback — public (browser
//!      redirect from TikTok), exchanges the authorization code for
//!      access/refresh tokens and stores them in fanbase_connections.
//!
//! Token storage: the OAuth tokens (access_token, refresh_token, open_id,
//! expires_at) are stored as a JSON blob in fanbase_connections.credential_ref.
//! The growth metric sync worker parses this blob, refreshes the access token
//! when expired, and calls /v2/user/info/ for follower_count.

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

/// Query parameters for the OAuth authorize redirect.
#[derive(Deserialize)]
pub struct AuthorizeParams {
    /// Where to redirect after successful connection. Defaults to the
    /// control plane connections page.
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
    // callback knows where to send the browser after success.
    let post_redirect = params.redirect.as_deref().unwrap_or("/connections");
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
/// exchanges the code for tokens, and stores them in fanbase_connections.
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
    let (state_uuid, post_redirect) = stored_state
        .split_once(':')
        .unwrap_or((stored_state, "/connections"));

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
    let http_client = reqwest::Client::new();
    let token_response = http_client
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
        Err(_) => return Problem::service_unavailable(request_id_value).into_response(),
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
        Err(_) => return Problem::service_unavailable(request_id_value).into_response(),
    };

    let data = match token_data.get("data") {
        Some(d) => d,
        None => return Problem::service_unavailable(request_id_value).into_response(),
    };

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
        return Problem::service_unavailable(request_id_value).into_response();
    }

    // Compute the access token expiry timestamp.
    let expires_at =
        time::OffsetDateTime::now_utc() + time::Duration::seconds(expires_in.saturating_sub(60));

    // Store tokens as JSON in credential_ref.
    let credential_json = serde_json::json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "open_id": open_id,
        "expires_at": expires_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    })
    .to_string();

    let workspace_id: uuid::Uuid = state.ops.workspace_id().into_uuid();

    // Store the connection via the fanbase repository (SQL stays in infra).
    let repo = crowdrelay_infra::fanbase::PostgresFanbaseRepository::new(state.database.clone());
    let label = format!("TikTok — {open_id}");
    if let Err(error) = repo
        .upsert_tiktok_connection(workspace_id, open_id, &credential_json, &label)
        .await
    {
        tracing::error!(error = %error, "failed to store TikTok connection");
        return Problem::service_unavailable(request_id_value).into_response();
    }

    tracing::info!(open_id = open_id, "TikTok connection established");

    // Clear the state cookie and redirect to the control plane.
    let clear_cookie = format!("{STATE_COOKIE}=; Max-Age=0; {STATE_COOKIE_FLAGS}");
    let redirect_url = format!("https://control.virya.music{post_redirect}");

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
