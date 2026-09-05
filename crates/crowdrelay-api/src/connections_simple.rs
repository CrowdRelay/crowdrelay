//! Simple API-key-based connection flow for Discord, Telegram, Last.fm,
//! Deezer, Discogs, Bluesky, Bandcamp, YouTube, Facebook, Instagram,
//! SoundCloud, and Reddit.
//!
//! Unlike TikTok (which uses OAuth), these platforms use simple credentials:
//! - Discord: just an invite code (Discord's own invite API, no key needed)
//! - Telegram: a channel username + bot token (Bot API)
//! - Last.fm: just an artist name (API key is a shared env var)
//! - Deezer: just a numeric artist ID (free public API, no key needed)
//! - Discogs: just a numeric artist ID (shared token for rate limits)
//! - Bluesky: just a handle (free public API, no key needed)
//! - Bandcamp: just a subdomain (HTML scrape, no key needed)
//! - YouTube: a channel ID (UC…), verified via Data API v3
//! - Facebook: a page ID (numeric), verified via Graph API
//! - Instagram: an IG Business account ID (numeric), verified via Graph API
//! - SoundCloud: a permalink (normalized), verified via HTML scrape
//! - Reddit: a subreddit name (normalized), verified via about.json
//!
//! YouTube, Facebook, Instagram, SoundCloud, and Reddit perform a
//! creation-time provider probe via `ProviderVerifier`. The probe result
//! is a diagnostic only — it is NOT persisted to the database and is NOT
//! a durable health state. If the probe proves the identity is invalid,
//! the connection is stored with `status = 'invalid'` so the sync worker
//! skips it. If the probe could not establish identity (Unavailable), the
//! connection is stored with `status = 'unverified'` — a successful sync
//! promotes it to `'connected'`.
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
    /// Discord invite code (e.g. `BBdDV6gVy` from `discord.gg/BBdDV6gVy`).
    /// Used for growth metric sync (member count from Discord's invite API).
    pub invite_code: String,
    /// Optional display label. Defaults to "Discord — {invite_code}".
    pub label: Option<String>,
    /// Optional bot token from the Discord Developer Portal. When provided
    /// alongside `channel_id`, the connection can post messages to the
    /// channel via the Bot API. The token is encrypted and stored in
    /// `encrypted_access_token`.
    pub bot_token: Option<String>,
    /// Optional Discord channel ID (numeric string). When provided alongside
    /// `bot_token`, the executor posts messages to this channel. Stored in
    /// `provider_account_id` (replacing the invite code, which remains in
    /// `external_account_ref` for metric sync).
    pub channel_id: Option<String>,
}

/// Creates or updates a Discord server connection for growth metric sync.
/// The invite code is stored in `external_account_ref`; the sync worker fetches
/// the member count from Discord's own invite API (no API key needed).
/// When `bot_token` and `channel_id` are provided, the connection also
/// supports posting messages to the channel via the Discord Bot API.
pub async fn create_discord_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateDiscordConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    // Discord invite codes are alphanumeric (case-sensitive). Strip a full
    // `discord.gg/` URL if the user pastes one instead of the bare code.
    let raw = body.invite_code.trim();
    let invite_code = raw.rsplit("discord.gg/").next().unwrap_or(raw);
    if !account_id_is_acceptable(invite_code, "discord")
        || !invite_code.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Discord — {invite_code}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    // Optional bot token + channel ID for posting. Both must be provided
    // together — a bot token without a channel ID has nothing to post to,
    // and a channel ID without a bot token has no way to post.
    let bot_token = body
        .bot_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let channel_id = body
        .channel_id
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let posting_config = match (bot_token, channel_id) {
        (Some(token), Some(channel)) => {
            // Discord bot tokens are long alphanumeric strings; channel IDs
            // are numeric strings (snowflake).
            if token.len() < 20 || !channel.chars().all(|c| c.is_ascii_digit()) {
                return Problem::bad_request(request_id_value).into_response();
            }
            Some((token.to_owned(), channel.to_owned()))
        }
        (None, None) => None,
        _ => {
            return Problem::bad_request(request_id_value).into_response();
        }
    };
    let repo = repository(&state);
    match repo
        .upsert_discord_connection(
            workspace(&state),
            invite_code,
            &label,
            posting_config
                .as_ref()
                .map(|(t, c)| (t.as_str(), c.as_str())),
        )
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
        .upsert_simple_connection(workspace(&state), "lastfm", artist, &label, false)
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
        .upsert_simple_connection(workspace(&state), "deezer", artist_id, &label, false)
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
        .upsert_simple_connection(workspace(&state), "discogs", artist_id, &label, false)
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
        .upsert_simple_connection(workspace(&state), "bluesky", handle, &label, false)
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
        .upsert_simple_connection(workspace(&state), "bandcamp", subdomain, &label, false)
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

// ---------------------------------------------------------------------------
// YouTube, Facebook, Instagram, SoundCloud, Reddit
//
// These five platforms have sync code in the growth metric sync worker but
// previously had no connection creation endpoints. Each handler validates
// and normalizes the identifier, probes the provider via a
// `ProviderVerifier` (behind a trait in `crowdrelay-infra`), and persists
// the connection with the correct status:
//   - Verified → status = 'connected'
//   - Unavailable → status = 'unverified' (sync worker tries it, promotes on success)
//   - Invalid → status = 'invalid' (sync worker skips it)
//
// The probe result (`verification`) is a creation-time diagnostic only.
// It is NOT persisted to the database and is NOT a durable health state.
// ---------------------------------------------------------------------------

/// Normalizes a SoundCloud permalink to its canonical form.
/// Accepts `virya`, `Virya`, `https://soundcloud.com/virya`,
/// `soundcloud.com/virya/` and returns `virya`.
fn normalize_soundcloud_permalink(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip protocol.
    let stripped = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    // Strip domain.
    let after_domain = stripped
        .strip_prefix("soundcloud.com/")
        .or_else(|| stripped.strip_prefix("www.soundcloud.com/"))
        .unwrap_or(stripped);
    // Strip trailing slash.
    let permalink = after_domain.trim_end_matches('/');
    // SoundCloud permalinks are URL-safe: alphanumeric, hyphens, underscores.
    if permalink.is_empty()
        || !permalink
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(permalink.to_owned())
}

/// Normalizes a Reddit subreddit name to its canonical form.
/// Accepts `Metal`, `r/Metal`, `/r/Metal/` and returns `Metal`.
/// Reddit subreddit names are 3-21 characters: alphanumeric + underscore.
fn normalize_reddit_subreddit(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip r/ or /r/ prefix.
    let stripped = trimmed
        .strip_prefix("/r/")
        .or_else(|| trimmed.strip_prefix("r/"))
        .unwrap_or(trimmed);
    // Strip trailing slash.
    let name = stripped.trim_end_matches('/');
    // Reddit subreddit names: 3-21 chars, alphanumeric + underscore.
    let len = name.chars().count();
    if !(3..=21).contains(&len) {
        return None;
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    Some(name.to_owned())
}

/// Persists a connection and returns the appropriate response based on the
/// verification result. Verified → `status = 'connected'`, Unavailable →
/// `status = 'unverified'`, Invalid → `status = 'invalid'`.
async fn persist_verified_connection(
    state: &crate::AppState,
    request_id_value: Option<String>,
    platform: &str,
    account_id: &str,
    label: &str,
    verification: crowdrelay_infra::provider_verification::VerificationResult,
) -> Response {
    let repo = repository(state);
    let is_invalid = verification.is_invalid();
    let is_unverified = matches!(
        &verification,
        crowdrelay_infra::provider_verification::VerificationResult::Unavailable { .. }
    );
    let persist_result = if is_invalid {
        repo.upsert_invalid_connection(workspace(state), platform, account_id, label)
            .await
    } else {
        repo.upsert_simple_connection(workspace(state), platform, account_id, label, is_unverified)
            .await
    };
    match persist_result {
        Ok(()) => {
            let status = if is_invalid {
                "invalid"
            } else if is_unverified {
                "unverified"
            } else {
                "connected"
            };
            let response_body = match &verification {
                crowdrelay_infra::provider_verification::VerificationResult::Verified {
                    display_name,
                } => {
                    serde_json::json!({
                        "platform": platform,
                        "status": status,
                        "verification": "verified",
                        "displayName": display_name,
                    })
                }
                crowdrelay_infra::provider_verification::VerificationResult::Invalid { reason } => {
                    serde_json::json!({
                        "platform": platform,
                        "status": status,
                        "verification": "invalid",
                        "reason": reason,
                    })
                }
                crowdrelay_infra::provider_verification::VerificationResult::Unavailable {
                    reason,
                } => {
                    serde_json::json!({
                        "platform": platform,
                        "status": status,
                        "verification": "unavailable",
                        "reason": reason,
                    })
                }
            };
            (
                StatusCode::CREATED,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(response_body),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(error = %error, platform = platform, "failed to create connection");
            Problem::internal(request_id_value).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateYoutubeConnectionRequest {
    /// YouTube channel ID (starts with UC…).
    pub channel_id: String,
    /// Optional display label. Defaults to "YouTube — {channel_id}".
    pub label: Option<String>,
}

/// Creates or updates a YouTube channel connection for growth metric sync.
/// The channel ID is verified via the Data API v3 at creation time. If the
/// probe proves the channel doesn't exist, the connection is stored with
/// `status = 'invalid'`.
pub async fn create_youtube_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateYoutubeConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let channel_id = body.channel_id.trim();
    // YouTube channel IDs start with UC and are alphanumeric + hyphens + underscores.
    if !account_id_is_acceptable(channel_id, "youtube")
        || !channel_id.starts_with("UC")
        || !channel_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("YouTube — {channel_id}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let verification = match &state.provider_verifiers.youtube {
        Some(verifier) => verifier.verify(channel_id).await,
        None => crowdrelay_infra::provider_verification::VerificationResult::Unavailable {
            reason: "YouTube API key not configured".to_owned(),
        },
    };
    persist_verified_connection(
        &state,
        request_id_value,
        "youtube",
        channel_id,
        &label,
        verification,
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateFacebookConnectionRequest {
    /// Facebook Page ID (numeric).
    pub page_id: String,
    /// Optional display label. Defaults to "Facebook — {page_id}".
    pub label: Option<String>,
}

/// Creates or updates a Facebook Page connection for growth metric sync.
/// The page ID is verified via the Graph API at creation time.
pub async fn create_facebook_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateFacebookConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let page_id = body.page_id.trim();
    if !account_id_is_acceptable(page_id, "facebook")
        || !page_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Facebook — {page_id}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let verification = match &state.provider_verifiers.facebook {
        Some(verifier) => verifier.verify(page_id).await,
        None => crowdrelay_infra::provider_verification::VerificationResult::Unavailable {
            reason: "Facebook Page access token not configured".to_owned(),
        },
    };
    persist_verified_connection(
        &state,
        request_id_value,
        "facebook",
        page_id,
        &label,
        verification,
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateInstagramConnectionRequest {
    /// Instagram Business account ID (numeric).
    pub ig_user_id: String,
    /// Optional display label. Defaults to "Instagram — {ig_user_id}".
    pub label: Option<String>,
}

/// Creates or updates an Instagram Business account connection for growth
/// metric sync. The IG user ID is verified via the Graph API at creation
/// time. Uses the same Facebook Page access token.
pub async fn create_instagram_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<
        Json<CreateInstagramConnectionRequest>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let ig_user_id = body.ig_user_id.trim();
    if !account_id_is_acceptable(ig_user_id, "instagram")
        || !ig_user_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Instagram — {ig_user_id}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let verification = match &state.provider_verifiers.instagram {
        Some(verifier) => verifier.verify(ig_user_id).await,
        None => crowdrelay_infra::provider_verification::VerificationResult::Unavailable {
            reason: "Facebook Page access token not configured".to_owned(),
        },
    };
    persist_verified_connection(
        &state,
        request_id_value,
        "instagram",
        ig_user_id,
        &label,
        verification,
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateSoundcloudConnectionRequest {
    /// SoundCloud permalink (e.g. "virya" or "https://soundcloud.com/virya").
    pub permalink: String,
    /// Optional display label. Defaults to "SoundCloud — {permalink}".
    pub label: Option<String>,
}

/// Creates or updates a SoundCloud artist connection for growth metric sync.
/// The permalink is normalized to canonical form and verified by fetching
/// the public artist page at creation time.
pub async fn create_soundcloud_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<
        Json<CreateSoundcloudConnectionRequest>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let Some(permalink) = normalize_soundcloud_permalink(&body.permalink) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if !account_id_is_acceptable(&permalink, "soundcloud") {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("SoundCloud — {permalink}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let verification = state.provider_verifiers.soundcloud.verify(&permalink).await;
    persist_verified_connection(
        &state,
        request_id_value,
        "soundcloud",
        &permalink,
        &label,
        verification,
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateRedditConnectionRequest {
    /// Subreddit name (e.g. "Metal", "r/Metal", "/r/Metal/").
    pub subreddit: String,
    /// Optional display label. Defaults to "Reddit — r/{subreddit}".
    pub label: Option<String>,
}

/// Creates or updates a Reddit subreddit connection for growth metric sync.
/// The subreddit name is normalized to canonical form and verified by
/// fetching about.json at creation time.
pub async fn create_reddit_connection(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateRedditConnectionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(Json(body)) = payload else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let Some(subreddit) = normalize_reddit_subreddit(&body.subreddit) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    if !account_id_is_acceptable(&subreddit, "reddit") {
        return Problem::bad_request(request_id_value).into_response();
    }
    let Some(label) = resolve_label(body.label, || format!("Reddit — r/{subreddit}")) else {
        return Problem::bad_request(request_id_value).into_response();
    };
    let verification = state.provider_verifiers.reddit.verify(&subreddit).await;
    persist_verified_connection(
        &state,
        request_id_value,
        "reddit",
        &subreddit,
        &label,
        verification,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_soundcloud_permalink_strips_url() {
        assert_eq!(
            normalize_soundcloud_permalink("https://soundcloud.com/virya"),
            Some("virya".to_owned())
        );
        assert_eq!(
            normalize_soundcloud_permalink("https://soundcloud.com/virya/"),
            Some("virya".to_owned())
        );
        assert_eq!(
            normalize_soundcloud_permalink("soundcloud.com/virya/"),
            Some("virya".to_owned())
        );
        assert_eq!(
            normalize_soundcloud_permalink("virya"),
            Some("virya".to_owned())
        );
        assert_eq!(
            normalize_soundcloud_permalink("Virya"),
            Some("Virya".to_owned())
        );
        assert_eq!(normalize_soundcloud_permalink(""), None);
        assert_eq!(normalize_soundcloud_permalink("  "), None);
    }

    #[test]
    fn normalize_reddit_subreddit_strips_prefix() {
        assert_eq!(
            normalize_reddit_subreddit("Metal"),
            Some("Metal".to_owned())
        );
        assert_eq!(
            normalize_reddit_subreddit("r/Metal"),
            Some("Metal".to_owned())
        );
        assert_eq!(
            normalize_reddit_subreddit("/r/Metal/"),
            Some("Metal".to_owned())
        );
        assert_eq!(
            normalize_reddit_subreddit("Metal"),
            Some("Metal".to_owned())
        );
        // Too short.
        assert_eq!(normalize_reddit_subreddit("ab"), None);
        // Too long.
        assert_eq!(
            normalize_reddit_subreddit("abcdefghijklmnopqrstuvwxyz"),
            None
        );
        // Invalid chars.
        assert_eq!(normalize_reddit_subreddit("metal-head"), None);
    }
}
