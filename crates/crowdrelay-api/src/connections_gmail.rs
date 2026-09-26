//! Gmail OAuth connection flow for contact import.
//!
//! Same shape as the Google Drive flow (`connections_gdrive`) — same
//! Google OAuth client (`CROWDRELAY_GOOGLE_ADS_CLIENT_ID` / `…_SECRET`),
//! same encrypted token storage — but a grant of its own:
//! `gmail.readonly` is a restricted scope, so connecting Drive must never
//! silently grant the mailbox. `access_type=offline&prompt=consent`
//! guarantees a refresh token on every connect, including reconnects.
//!
//! The contacts worker reads message *headers only* (From/To/Cc/Subject/
//! Date via format=metadata); the mailbox body is never requested.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{Problem, request_id};

const STATE_COOKIE_MAX_AGE: &str = "Max-Age=600";
const STATE_COOKIE_FLAGS: &str = "HttpOnly; Secure; SameSite=Lax; Path=/";

/// One Google grant: the connection platform its tokens are stored and
/// encrypted under, the scopes it asks for, and the state cookie that binds
/// the callback to the browser that started it. Each grant is separate on
/// purpose — connecting one must never silently grant another's scopes.
pub(crate) struct GoogleGrant {
    pub platform: &'static str,
    pub scopes: &'static str,
    pub state_cookie: &'static str,
    pub label: &'static str,
}

/// Read-only Gmail access + enough identity to name the account.
const GMAIL: GoogleGrant = GoogleGrant {
    platform: "gmail",
    scopes: "openid email https://www.googleapis.com/auth/gmail.readonly",
    state_cookie: "gmail_oauth_state",
    label: "Gmail",
};

const ALLOWED_POST_REDIRECTS: &[&str] = &[
    "/connections",
    "/connections/gmail",
    "/connections/youtube",
    "/audience",
    "/",
];

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

#[derive(Deserialize)]
pub(crate) struct AuthorizeParams {
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

/// Redirects the operator to Google's OAuth consent page for Gmail.
pub async fn authorize(
    State(state): State<crate::AppState>,
    Query(params): Query<AuthorizeParams>,
    headers: HeaderMap,
) -> Response {
    authorize_grant(&GMAIL, &state, params, &headers)
}

/// Handles the Gmail OAuth callback.
pub async fn callback(
    State(state): State<crate::AppState>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    callback_grant(&GMAIL, &state, params, &headers).await
}

pub(crate) fn authorize_grant(
    grant: &GoogleGrant,
    state: &crate::AppState,
    params: AuthorizeParams,
    headers: &HeaderMap,
) -> Response {
    let request_id_value = request_id(headers);
    let Some(client_id) = google_client_id() else {
        return Problem::service_unavailable(request_id_value).into_response();
    };
    let redirect_uri =
        crate::oauth_redirect::oauth_redirect_uri(state.public_api_origin.as_ref(), grant.platform);
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
        urlencoding(grant.scopes),
    );

    let cookie = format!(
        "{}={state_value}; {STATE_COOKIE_MAX_AGE}; {STATE_COOKIE_FLAGS}",
        grant.state_cookie
    );

    (
        StatusCode::FOUND,
        [
            (axum::http::header::LOCATION, google_url),
            (SET_COOKIE, cookie),
        ],
    )
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct CallbackParams {
    code: String,
    state: String,
}

/// Handles a grant's OAuth callback. Verifies the state cookie, exchanges
/// the code, resolves the Google user id via userinfo, and stores encrypted
/// tokens on the grant's platform.
pub(crate) async fn callback_grant(
    grant: &GoogleGrant,
    state: &crate::AppState,
    params: CallbackParams,
    headers: &HeaderMap,
) -> Response {
    let request_id_value = request_id(headers);

    let cookie_value = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .find_map(|c| c.trim().strip_prefix(&format!("{}=", grant.state_cookie)))
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
    let redirect_uri =
        crate::oauth_redirect::oauth_redirect_uri(state.public_api_origin.as_ref(), grant.platform);

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
        tracing::warn!(
            has_access_token = !access_token.is_empty(),
            has_refresh_token = !refresh_token.is_empty(),
            "Google token response missing access_token or refresh_token"
        );
        return Problem::service_unavailable(request_id_value).into_response();
    }

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
        .upsert_google_grant(
            grant.platform,
            workspace_id,
            &google_user_id,
            access_token,
            refresh_token,
            expires_at,
            grant.scopes,
            grant.label,
        )
        .await
    {
        tracing::error!(error = %error, platform = grant.platform, "failed to store Google connection");
        return Problem::service_unavailable(request_id_value).into_response();
    }

    tracing::info!(google_user_id = %google_user_id, platform = grant.platform, "Google connection established");

    let validated_redirect = validate_post_redirect(post_redirect);
    let clear_cookie = format!("{}=; Max-Age=0; {STATE_COOKIE_FLAGS}", grant.state_cookie);
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

fn urlencoding(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace(':', "%3A")
        .replace('/', "%2F")
}
