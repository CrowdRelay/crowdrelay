//! YouTube OAuth grant for answering comments on the band's own videos.
//!
//! The Gmail flow's generic grant (`connections_gmail::GoogleGrant`) with a
//! grant of its own: `youtube.force-ssl` is the scope Google requires to post
//! a comment reply, and connecting Gmail or Drive must never grant it. Tokens
//! live on platform `youtube_account`, apart from the `youtube` rows the
//! video sync keys by channel id. Reading comments needs no grant — the
//! worker reads them with the API key the video sync already holds.

use axum::{
    Router,
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
    routing::get,
};

use crate::connections_gmail::{
    AuthorizeParams, CallbackParams, GoogleGrant, authorize_grant, callback_grant,
};

const YOUTUBE: GoogleGrant = GoogleGrant {
    platform: "youtube_account",
    scopes: "openid email https://www.googleapis.com/auth/youtube.force-ssl",
    state_cookie: "youtube_oauth_state",
    label: "YouTube (replies)",
};

/// Redirects the operator to Google's consent page for the YouTube grant.
pub async fn authorize(
    State(state): State<crate::AppState>,
    Query(params): Query<AuthorizeParams>,
    headers: HeaderMap,
) -> Response {
    authorize_grant(&YOUTUBE, &state, params, &headers)
}

/// Handles the YouTube grant's OAuth callback.
pub async fn callback(
    State(state): State<crate::AppState>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    callback_grant(&YOUTUBE, &state, params, &headers).await
}

/// The grant's two public routes, merged into the public router.
pub(crate) fn public_routes() -> Router<crate::AppState> {
    Router::new()
        .route(
            "/v1/public/connections/youtube_account/authorize",
            get(authorize),
        )
        .route(
            "/v1/public/connections/youtube_account/callback",
            get(callback),
        )
}
