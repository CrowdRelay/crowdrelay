//! Simple API-key-based connection flow for Discord, Telegram, Last.fm,
//! Deezer, Discogs, Bluesky, and Bandcamp.
//!
//! Unlike TikTok (which uses OAuth), these platforms use simple credentials:
//! - Discord: just a server ID (disdex.io is a free public API, no key needed)
//! - Telegram: a channel username + bot token (Bot API)
//! - Last.fm: just an artist name (API key is a shared env var)
//! - Deezer: just a numeric artist ID (free public API, no key needed)
//! - Discogs: just a numeric artist ID (shared token for rate limits)
//! - Bluesky: just a handle (free public API, no key needed)
//! - Bandcamp: just a subdomain (HTML scrape, no key needed)
//!
//! All endpoints are admin-authenticated (operator-only).

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_infra::fanbase::PostgresFanbaseRepository;
use serde::Deserialize;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

fn workspace(state: &crate::AppState) -> Uuid {
    state.ticketing.workspace_id().into_uuid()
}

fn repository(state: &crate::AppState) -> PostgresFanbaseRepository {
    PostgresFanbaseRepository::new(state.database.clone())
        .with_encryption_key(state.response_encryption_key.clone())
}

/// `fanbase_connections` caps `external_account_ref`, `credential_ref` and
/// `label` at 200 characters and rejects blank ones. Validating here keeps a
/// bad body a 400 instead of a constraint violation surfacing as a 500.
const MAX_FIELD_LEN: usize = 200;

/// Resolves the caller's label or the supplied default, rejecting a label the
/// column would not accept. `credential_ref` is `{platform}:{account_id}`, so
/// the account id is bounded a little tighter than the column itself.
fn resolve_label(label: Option<String>, default: impl FnOnce() -> String) -> Option<String> {
    let label = label.map_or_else(default, |value| value.trim().to_owned());
    (!label.is_empty() && label.chars().count() <= MAX_FIELD_LEN).then_some(label)
}

/// Whether an account identifier fits both `external_account_ref` and the
/// `{platform}:{account_id}` credential reference derived from it.
fn account_id_is_acceptable(account_id: &str, platform: &str) -> bool {
    !account_id.is_empty()
        && account_id.chars().count() <= MAX_FIELD_LEN
        && platform.len() + 1 + account_id.chars().count() <= MAX_FIELD_LEN
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateDiscordConnectionRequest {
    /// Discord server (guild) ID — a numeric snowflake.
    pub guild_id: String,
    /// Optional display label. Defaults to "Discord — {guild_id}".
    pub label: Option<String>,
}

/// Creates or updates a Discord server connection for growth metric sync.
/// The guild ID is stored in `provider_account_id`; the sync worker fetches
/// the member count from disdex.io (free, no API key).
pub async fn create_discord_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateDiscordConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    // A guild ID is a snowflake: ASCII digits only. Anything else is a caller
    // mistake that would otherwise surface as a 404 from disdex.io hours later.
    let guild_id = body.guild_id.trim();
    if !account_id_is_acceptable(guild_id, "discord")
        || !guild_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Discord — {guild_id}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let repo = repository(&state);
    match repo
        .upsert_discord_connection(workspace(&state), guild_id, &label)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "platform": "discord", "status": "connected" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to create Discord connection");
            Problem::internal(request_id_value).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateTelegramConnectionRequest {
    /// Telegram channel username (e.g. `@virya_music`).
    pub channel: String,
    /// Bot token from @BotFather.
    pub bot_token: String,
    /// Optional display label. Defaults to "Telegram — {channel}".
    pub label: Option<String>,
}

/// Creates or updates a Telegram channel connection for growth metric sync.
/// The bot token is encrypted and stored; the sync worker fetches the
/// subscriber count via the Bot API getChatMemberCount endpoint.
pub async fn create_telegram_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateTelegramConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let channel = body.channel.trim();
    let bot_token = body.bot_token.trim();
    if !account_id_is_acceptable(channel, "telegram") || bot_token.is_empty() {
        return Problem::bad_request(request_id_value).into_response();
    }
    // Basic validation: Telegram bot tokens are numeric:alphanumeric.
    if !bot_token.contains(':') {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Telegram — {channel}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let repo = repository(&state);
    match repo
        .upsert_telegram_connection(workspace(&state), channel, bot_token, &label)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "platform": "telegram", "status": "connected" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to create Telegram connection");
            Problem::internal(request_id_value).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateLastfmConnectionRequest {
    /// Last.fm artist name (canonical spelling).
    pub artist: String,
    /// Optional display label. Defaults to "Last.fm — {artist}".
    pub label: Option<String>,
}

/// Creates or updates a Last.fm artist connection for growth metric sync.
/// The artist name is stored in `provider_account_id`; the sync worker
/// fetches listener and play counts via the official Last.fm API
/// (artist.getInfo). The API key is a shared env var — no per-connection
/// secret is needed.
pub async fn create_lastfm_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateLastfmConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let artist = body.artist.trim();
    if !account_id_is_acceptable(artist, "lastfm") {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Last.fm — {artist}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let repo = repository(&state);
    match repo
        .upsert_simple_connection(workspace(&state), "lastfm", artist, &label)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "platform": "lastfm", "status": "connected" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to create Last.fm connection");
            Problem::internal(request_id_value).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateDeezerConnectionRequest {
    /// Deezer artist ID (numeric, from the Deezer URL or API).
    pub artist_id: String,
    /// Optional display label. Defaults to "Deezer — {artist_id}".
    pub label: Option<String>,
}

/// Creates or updates a Deezer artist connection for growth metric sync.
/// The numeric artist ID is stored in `provider_account_id`; the sync worker
/// fetches the fan count via the free Deezer API (api.deezer.com/artist/{id}).
/// No API key is needed — the endpoint is unauthenticated.
pub async fn create_deezer_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateDeezerConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    // Deezer artist IDs are numeric.
    let artist_id = body.artist_id.trim();
    if !account_id_is_acceptable(artist_id, "deezer")
        || !artist_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Deezer — {artist_id}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let repo = repository(&state);
    match repo
        .upsert_simple_connection(workspace(&state), "deezer", artist_id, &label)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "platform": "deezer", "status": "connected" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to create Deezer connection");
            Problem::internal(request_id_value).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateDiscogsConnectionRequest {
    /// Discogs artist ID (numeric, from the Discogs URL or API).
    pub artist_id: String,
    /// Optional display label. Defaults to "Discogs — {artist_id}".
    pub label: Option<String>,
}

/// Creates or updates a Discogs artist connection for growth metric sync.
/// The numeric artist ID is stored in `provider_account_id`; the sync worker
/// fetches collection and wantlist counts via the Discogs API
/// (api.discogs.com/artists/{id}). A shared personal access token
/// (CROWDRELAY_DISCOGS_TOKEN) is used for higher rate limits.
pub async fn create_discogs_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateDiscogsConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let artist_id = body.artist_id.trim();
    if !account_id_is_acceptable(artist_id, "discogs")
        || !artist_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Discogs — {artist_id}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let repo = repository(&state);
    match repo
        .upsert_simple_connection(workspace(&state), "discogs", artist_id, &label)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "platform": "discogs", "status": "connected" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to create Discogs connection");
            Problem::internal(request_id_value).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateBlueskyConnectionRequest {
    /// Bluesky handle (e.g. "virya.bsky.social").
    pub handle: String,
    /// Optional display label. Defaults to "Bluesky — {handle}".
    pub label: Option<String>,
}

/// Creates or updates a Bluesky actor connection for growth metric sync.
/// The handle is stored in `provider_account_id`; the sync worker fetches
/// the follower count via the free public Bluesky API
/// (public.api.bsky.app/xrpc/app.bsky.actor.getProfile). No API key is needed.
pub async fn create_bluesky_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateBlueskyConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let handle = body.handle.trim();
    if !account_id_is_acceptable(handle, "bluesky") {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Bluesky — {handle}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let repo = repository(&state);
    match repo
        .upsert_simple_connection(workspace(&state), "bluesky", handle, &label)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "platform": "bluesky", "status": "connected" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to create Bluesky connection");
            Problem::internal(request_id_value).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateBandcampConnectionRequest {
    /// Bandcamp subdomain (e.g. "virya" for virya.bandcamp.com).
    pub subdomain: String,
    /// Optional display label. Defaults to "Bandcamp — {subdomain}".
    pub label: Option<String>,
}

/// Creates or updates a Bandcamp artist connection for growth metric sync.
/// The subdomain is stored in `provider_account_id`; the sync worker scrapes
/// the community page HTML to count recent supporters. No API key is needed
/// — Bandcamp has no public API.
pub async fn create_bandcamp_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateBandcampConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let subdomain = body.subdomain.trim();
    // Bandcamp subdomains are lowercase alphanumeric + hyphens.
    if !account_id_is_acceptable(subdomain, "bandcamp")
        || !subdomain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || subdomain.starts_with('-')
        || subdomain.ends_with('-')
    {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Bandcamp — {subdomain}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let repo = repository(&state);
    match repo
        .upsert_simple_connection(workspace(&state), "bandcamp", subdomain, &label)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(serde_json::json!({ "platform": "bandcamp", "status": "connected" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "failed to create Bandcamp connection");
            Problem::internal(request_id_value).into_response()
        }
    }
}
