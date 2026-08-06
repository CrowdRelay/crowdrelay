//! Canonical VIRYA AREA drop catalogue, player progress, challenges, and claims.
//!
//! Exact claim coordinates never leave this module. Public and player-facing
//! responses contain only coarse city coordinates suitable for navigation.

use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, acquisition::fan_session_from_headers};

const PRIVATE_NO_STORE: &str = "private, no-store";
const PUBLIC_AREA_CACHE: &str = "public, max-age=15, s-maxage=30, stale-while-revalidate=60";
const CHALLENGE_LIFETIME_SECONDS: i64 = 90;
const CHALLENGE_MIN_DURATION_MS: i64 = 6_000;
const CHALLENGE_MIN_SAMPLES: usize = 3;
const CHALLENGE_MAX_SAMPLES: usize = 8;
const CHALLENGE_WINDOW_SECONDS: i64 = 10 * 60;
const MAX_CHALLENGES_PER_WINDOW: i64 = 8;
const MAX_ACCURACY_METERS: f64 = 60.0;
const SAMPLE_CLOCK_TOLERANCE_MS: i64 = 120_000;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AreaError {
    error: &'static str,
    code: &'static str,
}

fn error_response(status: StatusCode, code: &'static str, error: &'static str) -> Response {
    (
        status,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(AreaError { error, code }),
    )
        .into_response()
}

fn temporary() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "TEMPORARY",
        "AREA is temporarily unavailable.",
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LinkPlayerRequest {
    email: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkPlayerResponse {
    player_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChallengeRequest {
    drop_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChallengeResponse {
    ok: bool,
    challenge: String,
    issued_at: u64,
    expires_at: u64,
    min_samples: u32,
    max_samples: u32,
    min_duration_ms: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PositionSample {
    lat: f64,
    lng: f64,
    accuracy: f64,
    captured_at: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimRequest {
    drop_id: String,
    challenge: String,
    samples: Vec<PositionSample>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyClaimRequest {
    drop_id: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    claimed_at: Option<OffsetDateTime>,
    #[serde(default)]
    edition_number: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportClaimsRequest {
    claims: Vec<LegacyClaimRequest>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicDrop {
    id: String,
    number: String,
    city: String,
    region: String,
    signal_city_slug: String,
    map_x: i16,
    map_y: i16,
    approximate_lat: f64,
    approximate_lng: f64,
    clue: DropClue,
    active: bool,
    full: bool,
    claimed: bool,
}

#[derive(Clone, Debug, Serialize)]
struct DropClue {
    en: String,
    pl: String,
}

#[derive(Clone, Debug, Serialize)]
struct LiveDrop {
    id: String,
}

#[derive(Clone, Debug, Default, Serialize)]
struct AreaCommunity {
    current: u32,
    total: u32,
    percent: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AreaClaim {
    drop_id: String,
    number: String,
    city: String,
    line: String,
    track: String,
    edition: String,
    riddle: String,
    #[serde(with = "time::serde::rfc3339")]
    claimed_at: OffsetDateTime,
    distance_meters: u32,
    edition_number: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AreaCollectible {
    drop_id: String,
    number: String,
    city: String,
    line: String,
    track: String,
    edition: String,
    riddle: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AreaWallet {
    authenticated: bool,
    migration_required: bool,
    token_balance: u32,
    reward_credits: u32,
    reward: RewardSummary,
    collection_size: u32,
    community: AreaCommunity,
    claims: Vec<AreaClaim>,
    vouchers: Vec<serde_json::Value>,
    live_drops: Vec<LiveDrop>,
    drops: Vec<PublicDrop>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RewardSummary {
    credits_per_code: u32,
    benefit: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponse {
    ok: bool,
    already_claimed: bool,
    collectible: Option<AreaCollectible>,
    reward_credits_awarded: u32,
}

#[derive(Debug, Serialize)]
struct PublicDropsResponse {
    items: Vec<PublicDrop>,
    community: AreaCommunity,
}

#[derive(Debug, FromRow)]
struct DropRow {
    id: String,
    number: String,
    city: String,
    region: String,
    signal_city_slug: String,
    map_x: i16,
    map_y: i16,
    approximate_lat: f64,
    approximate_lng: f64,
    exact_lat: Option<f64>,
    exact_lng: Option<f64>,
    radius_meters: i32,
    max_claims: i32,
    starts_at: OffsetDateTime,
    ends_at: OffsetDateTime,
    clue_en: String,
    clue_pl: String,
    collectible_line: String,
    collectible_track: String,
    collectible_edition: String,
    collectible_riddle: String,
    active_now: bool,
    claim_count: i64,
    player_claimed: bool,
}

#[derive(Debug, FromRow)]
struct ClaimRow {
    drop_id: String,
    number: String,
    city: String,
    collectible_line: String,
    collectible_track: String,
    collectible_edition: String,
    collectible_riddle: String,
    claimed_at: OffsetDateTime,
    distance_meters: i32,
    edition_number: i32,
}

#[derive(Debug, FromRow)]
struct LockedChallenge {
    issued_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    drop_id: String,
}

#[derive(Debug, FromRow)]
struct ExistingClaim {
    drop_id: String,
    number: String,
    city: String,
    collectible_line: String,
    collectible_track: String,
    collectible_edition: String,
    collectible_riddle: String,
}

fn normalize_email(raw: &str) -> Option<String> {
    if raw.chars().any(char::is_control) {
        return None;
    }
    let email = raw.trim().to_lowercase();
    let valid = email.len() <= 320
        && email.len() >= 3
        && email.matches('@').count() == 1
        && !email.starts_with('@')
        && !email.ends_with('@');
    valid.then_some(email)
}

fn valid_drop_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7
        && bytes.iter().take(3).all(u8::is_ascii_lowercase)
        && bytes.get(3) == Some(&b'-')
        && bytes.iter().skip(4).all(u8::is_ascii_digit)
}

fn valid_idempotency_key(headers: &HeaderMap) -> bool {
    headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| Uuid::parse_str(value).is_ok())
}

fn epoch_millis(value: OffsetDateTime) -> u64 {
    let millis = value.unix_timestamp_nanos() / 1_000_000;
    u64::try_from(millis).unwrap_or_default()
}

fn challenge_token() -> String {
    let first = Uuid::now_v7().simple().to_string();
    let second = Uuid::now_v7().simple().to_string();
    format!("{first}.{second}")
}

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn public_drop(row: &DropRow) -> PublicDrop {
    let full = row.claim_count >= i64::from(row.max_claims);
    PublicDrop {
        id: row.id.clone(),
        number: row.number.clone(),
        city: row.city.clone(),
        region: row.region.clone(),
        signal_city_slug: row.signal_city_slug.clone(),
        map_x: row.map_x,
        map_y: row.map_y,
        approximate_lat: row.approximate_lat,
        approximate_lng: row.approximate_lng,
        clue: DropClue {
            en: row.clue_en.clone(),
            pl: row.clue_pl.clone(),
        },
        active: row.active_now,
        full,
        claimed: row.player_claimed,
    }
}

fn collectible_from_existing(row: &ExistingClaim) -> AreaCollectible {
    AreaCollectible {
        drop_id: row.drop_id.clone(),
        number: row.number.clone(),
        city: row.city.clone(),
        line: row.collectible_line.clone(),
        track: row.collectible_track.clone(),
        edition: row.collectible_edition.clone(),
        riddle: row.collectible_riddle.clone(),
    }
}

async fn load_drops(
    state: &crate::AppState,
    player_id: Option<Uuid>,
) -> Result<Vec<DropRow>, sqlx::Error> {
    sqlx::query_as::<_, DropRow>(
        r#"
        SELECT
            area_drop.id,
            area_drop.number,
            area_drop.city,
            area_drop.region,
            area_drop.signal_city_slug,
            area_drop.map_x,
            area_drop.map_y,
            area_drop.approximate_lat,
            area_drop.approximate_lng,
            area_drop.exact_lat,
            area_drop.exact_lng,
            area_drop.radius_meters,
            area_drop.max_claims,
            area_drop.starts_at,
            area_drop.ends_at,
            area_drop.clue_en,
            area_drop.clue_pl,
            area_drop.collectible_line,
            area_drop.collectible_track,
            area_drop.collectible_edition,
            area_drop.collectible_riddle,
            area_drop.active
                AND area_drop.exact_lat IS NOT NULL
                AND area_drop.exact_lng IS NOT NULL
                AND area_drop.starts_at <= now()
                AND area_drop.ends_at >= now() AS active_now,
            (
                SELECT count(*)::bigint
                FROM area_claims AS claim
                WHERE claim.workspace_id = area_drop.workspace_id
                  AND claim.drop_id = area_drop.id
            ) AS claim_count,
            CASE
                WHEN $2::uuid IS NULL THEN false
                ELSE EXISTS (
                    SELECT 1
                    FROM area_claims AS player_claim
                    WHERE player_claim.workspace_id = area_drop.workspace_id
                      AND player_claim.drop_id = area_drop.id
                      AND player_claim.player_id = $2
                )
            END AS player_claimed
        FROM area_drops AS area_drop
        WHERE area_drop.workspace_id = $1
        ORDER BY area_drop.number, area_drop.id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_all(state.ticketing.pool())
    .await
}

async fn load_claims(
    state: &crate::AppState,
    player_id: Uuid,
) -> Result<Vec<AreaClaim>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ClaimRow>(
        r#"
        SELECT
            claim.drop_id,
            area_drop.number,
            area_drop.city,
            area_drop.collectible_line,
            area_drop.collectible_track,
            area_drop.collectible_edition,
            area_drop.collectible_riddle,
            claim.claimed_at,
            claim.distance_meters,
            claim.edition_number
        FROM area_claims AS claim
        INNER JOIN area_drops AS area_drop
          ON area_drop.workspace_id = claim.workspace_id
         AND area_drop.id = claim.drop_id
        WHERE claim.workspace_id = $1
          AND claim.player_id = $2
        ORDER BY claim.claimed_at, claim.drop_id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_all(state.ticketing.pool())
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let edition_number = u32::try_from(row.edition_number).ok();
            AreaClaim {
                drop_id: row.drop_id,
                number: row.number,
                city: row.city,
                line: row.collectible_line,
                track: row.collectible_track,
                edition: row.collectible_edition,
                riddle: row.collectible_riddle,
                claimed_at: row.claimed_at,
                distance_meters: u32::try_from(row.distance_meters).unwrap_or_default(),
                edition_number,
            }
        })
        .collect())
}

async fn wallet_for_player(
    state: &crate::AppState,
    player_id: Uuid,
) -> Result<AreaWallet, sqlx::Error> {
    let drops = load_drops(state, Some(player_id)).await?;
    let claims = load_claims(state, player_id).await?;
    let total = u32::try_from(drops.len()).unwrap_or(u32::MAX);
    let current =
        u32::try_from(drops.iter().filter(|drop| drop.claim_count > 0).count()).unwrap_or(u32::MAX);
    let percent = if total == 0 {
        0.0
    } else {
        f64::from(current) * 100.0 / f64::from(total)
    };
    let token_balance = u32::try_from(claims.len()).unwrap_or(u32::MAX);
    let public_drops = drops.iter().map(public_drop).collect::<Vec<_>>();
    let live_drops = public_drops
        .iter()
        .filter(|drop| drop.active && !drop.full)
        .map(|drop| LiveDrop {
            id: drop.id.clone(),
        })
        .collect();

    Ok(AreaWallet {
        authenticated: true,
        migration_required: false,
        token_balance,
        reward_credits: token_balance,
        reward: RewardSummary {
            credits_per_code: 1,
            benefit: "free-item-and-shipping",
        },
        collection_size: total,
        community: AreaCommunity {
            current,
            total,
            percent,
        },
        claims,
        vouchers: Vec::new(),
        live_drops,
        drops: public_drops,
    })
}

async fn upsert_player(
    state: &crate::AppState,
    normalized_email: &str,
    fan_id: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO area_players (workspace_id, normalized_email, fan_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (workspace_id, normalized_email) DO UPDATE
        SET fan_id = COALESCE(area_players.fan_id, EXCLUDED.fan_id),
            last_seen_at = now()
        RETURNING id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(normalized_email)
    .bind(fan_id)
    .fetch_one(state.ticketing.pool())
    .await
}

async fn mobile_player(
    state: &crate::AppState,
    headers: &HeaderMap,
) -> Result<Option<Uuid>, sqlx::Error> {
    let Some(session) = fan_session_from_headers(headers) else {
        return Ok(None);
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let fan_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        UPDATE fan_sessions
        SET last_seen_at = now()
        WHERE workspace_id = $1
          AND session_token_hash = digest($2, 'sha256')
          AND revoked_at IS NULL
          AND expires_at > now()
        RETURNING fan_id
        "#,
    )
    .bind(workspace_id)
    .bind(session.as_str())
    .fetch_optional(state.ticketing.pool())
    .await?;
    let Some(fan_id) = fan_id else {
        return Ok(None);
    };
    let email = sqlx::query_scalar::<_, String>(
        r#"
        SELECT normalized_email
        FROM fans
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'active'
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .fetch_optional(state.ticketing.pool())
    .await?;
    match email {
        Some(email) => upsert_player(state, &email, Some(fan_id)).await.map(Some),
        None => Ok(None),
    }
}

async fn player_exists(state: &crate::AppState, player_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM area_players
            WHERE workspace_id = $1 AND id = $2
        )
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_one(state.ticketing.pool())
    .await
}

/// Returns the public 12-drop catalogue without exact claim coordinates.
pub async fn public_drops(State(state): State<crate::AppState>) -> Response {
    match load_drops(&state, None).await {
        Ok(rows) => {
            let total = u32::try_from(rows.len()).unwrap_or(u32::MAX);
            let current = u32::try_from(rows.iter().filter(|drop| drop.claim_count > 0).count())
                .unwrap_or(u32::MAX);
            let percent = if total == 0 {
                0.0
            } else {
                f64::from(current) * 100.0 / f64::from(total)
            };
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PUBLIC_AREA_CACHE)],
                Json(PublicDropsResponse {
                    items: rows.iter().map(public_drop).collect(),
                    community: AreaCommunity {
                        current,
                        total,
                        percent,
                    },
                }),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "AREA public catalogue unavailable");
            temporary()
        }
    }
}

/// Links a website AREA account to the same canonical player identity as a fan session.
pub async fn link_player(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<LinkPlayerRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    if !valid_idempotency_key(&headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "A valid Idempotency-Key is required.",
        );
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Invalid request.",
            );
        }
    };
    let Some(email) = normalize_email(&payload.email) else {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_REQUEST",
            "Invalid email.",
        );
    };
    match upsert_player(&state, &email, None).await {
        Ok(player_id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(LinkPlayerResponse { player_id }),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "AREA player link failed");
            temporary()
        }
    }
}

/// Returns the AREA wallet for an authenticated Virya Signal fan session.
pub async fn me_wallet(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    match mobile_player(&state, &headers).await {
        Ok(Some(player_id)) => match wallet_for_player(&state, player_id).await {
            Ok(wallet) => (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(wallet),
            )
                .into_response(),
            Err(error) => {
                tracing::error!(%error, "AREA mobile wallet failed");
                temporary()
            }
        },
        Ok(None) => error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_REQUIRED",
            "Sign in required.",
        ),
        Err(error) => {
            tracing::warn!(%error, "AREA fan session lookup failed");
            temporary()
        }
    }
}

/// Returns the AREA wallet for a linked website player.
pub async fn internal_wallet(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    match player_exists(&state, player_id).await {
        Ok(true) => match wallet_for_player(&state, player_id).await {
            Ok(wallet) => (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(wallet),
            )
                .into_response(),
            Err(error) => {
                tracing::error!(%error, "AREA internal wallet failed");
                temporary()
            }
        },
        Ok(false) => error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA player lookup failed");
            temporary()
        }
    }
}

async fn next_edition_number(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    drop_id: &str,
    max_claims: i32,
    preferred: Option<u32>,
) -> Result<Option<i32>, sqlx::Error> {
    if let Some(preferred) = preferred
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| (1..=max_claims).contains(value))
    {
        let available = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT NOT EXISTS (
                SELECT 1
                FROM area_claims
                WHERE workspace_id = $1
                  AND drop_id = $2
                  AND edition_number = $3
            )
            "#,
        )
        .bind(workspace_id)
        .bind(drop_id)
        .bind(preferred)
        .fetch_one(&mut **transaction)
        .await?;
        if available {
            return Ok(Some(preferred));
        }
    }

    sqlx::query_scalar::<_, i32>(
        r#"
        SELECT candidate::integer
        FROM generate_series(1, $3::integer) AS candidate
        WHERE NOT EXISTS (
            SELECT 1
            FROM area_claims
            WHERE workspace_id = $1
              AND drop_id = $2
              AND edition_number = candidate
        )
        ORDER BY candidate
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(drop_id)
    .bind(max_claims)
    .fetch_optional(&mut **transaction)
    .await
}

/// Imports claims created by the pre-backend website ledger.
///
/// This route is internal and idempotent. It preserves original timestamps and
/// edition numbers where they are still available, while never exceeding a
/// drop's canonical capacity.
pub async fn internal_import_claims(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ImportClaimsRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    if !valid_idempotency_key(&headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "A valid Idempotency-Key is required.",
        );
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Invalid import request.",
            );
        }
    };
    if payload.claims.len() > 12
        || payload.claims.iter().any(|claim| {
            !valid_drop_id(&claim.drop_id)
                || claim
                    .edition_number
                    .is_some_and(|number| number == 0 || number > 500)
                || claim
                    .claimed_at
                    .is_some_and(|claimed_at| claimed_at > OffsetDateTime::now_utc())
        })
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Invalid legacy claims.",
        );
    }
    let mut unique = HashSet::with_capacity(payload.claims.len());
    if payload
        .claims
        .iter()
        .any(|claim| !unique.insert(claim.drop_id.clone()))
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Duplicate legacy claims are not allowed.",
        );
    }
    match player_exists(&state, player_id).await {
        Ok(true) => {}
        Ok(false) => {
            return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found.");
        }
        Err(error) => {
            tracing::error!(%error, "AREA legacy import player lookup failed");
            return temporary();
        }
    }

    let mut claims = payload.claims;
    claims.sort_by(|left, right| left.drop_id.cmp(&right.drop_id));
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => {
            tracing::error!(%error, "AREA legacy import transaction failed to start");
            return temporary();
        }
    };

    for claim in claims {
        let drop = match lock_drop(&mut transaction, workspace_id, &claim.drop_id, player_id).await
        {
            Ok(Some(drop)) => drop,
            Ok(None) => {
                return error_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "DROP_UNKNOWN",
                    "A legacy drop no longer exists.",
                );
            }
            Err(error) => {
                tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy import drop lock failed");
                return temporary();
            }
        };
        match existing_claim(&mut transaction, workspace_id, player_id, &claim.drop_id).await {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(error) => {
                tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy import claim lookup failed");
                return temporary();
            }
        }
        let edition_number = match next_edition_number(
            &mut transaction,
            workspace_id,
            &claim.drop_id,
            drop.max_claims,
            claim.edition_number,
        )
        .await
        {
            Ok(Some(number)) => number,
            Ok(None) => {
                return error_response(
                    StatusCode::CONFLICT,
                    "DROP_FULL",
                    "A legacy drop has reached its claim limit.",
                );
            }
            Err(error) => {
                tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy edition allocation failed");
                return temporary();
            }
        };
        let now = OffsetDateTime::now_utc();
        let fallback_claimed_at = now.max(drop.starts_at).min(drop.ends_at);
        let claimed_at = claim
            .claimed_at
            .filter(|value| *value >= drop.starts_at && *value <= drop.ends_at)
            .unwrap_or(fallback_claimed_at);
        let inserted = sqlx::query(
            r#"
            INSERT INTO area_claims (
                workspace_id, player_id, drop_id, claimed_at,
                distance_meters, edition_number, claim_source
            )
            VALUES ($1, $2, $3, $4, 0, $5, 'legacy_import')
            ON CONFLICT (workspace_id, player_id, drop_id) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(player_id)
        .bind(&claim.drop_id)
        .bind(claimed_at)
        .bind(edition_number)
        .execute(&mut *transaction)
        .await;
        match inserted {
            Ok(result) if result.rows_affected() <= 1 => {}
            Ok(_) => return temporary(),
            Err(error) => {
                tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy import insert failed");
                return temporary();
            }
        }
    }

    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "AREA legacy import commit failed");
        return temporary();
    }
    match wallet_for_player(&state, player_id).await {
        Ok(wallet) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(wallet),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "AREA wallet reload after legacy import failed");
            temporary()
        }
    }
}

async fn issue_challenge(
    state: &crate::AppState,
    player_id: Uuid,
    headers: &HeaderMap,
    payload: Result<Json<ChallengeRequest>, JsonRejection>,
) -> Response {
    if !valid_idempotency_key(headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "A valid Idempotency-Key is required.",
        );
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Invalid request.",
            );
        }
    };
    if !valid_drop_id(&payload.drop_id) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid drop.");
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let active = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM area_drops
            WHERE workspace_id = $1
              AND id = $2
              AND active
              AND exact_lat IS NOT NULL
              AND exact_lng IS NOT NULL
              AND starts_at <= now()
              AND ends_at >= now()
              AND (
                  SELECT count(*)
                  FROM area_claims
                  WHERE area_claims.workspace_id = area_drops.workspace_id
                    AND area_claims.drop_id = area_drops.id
              ) < max_claims
        )
        "#,
    )
    .bind(workspace_id)
    .bind(&payload.drop_id)
    .fetch_one(state.ticketing.pool())
    .await;
    match active {
        Ok(true) => {}
        Ok(false) => {
            return error_response(
                StatusCode::CONFLICT,
                "DROP_INACTIVE",
                "This drop is not active.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "AREA challenge drop lookup failed");
            return temporary();
        }
    }

    let recent = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM area_challenges
        WHERE workspace_id = $1
          AND player_id = $2
          AND issued_at > now() - ($3::bigint * interval '1 second')
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(CHALLENGE_WINDOW_SECONDS)
    .fetch_one(state.ticketing.pool())
    .await;
    match recent {
        Ok(count) if count >= MAX_CHALLENGES_PER_WINDOW => {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "Too many attempts. Try again later.",
            );
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, "AREA challenge rate lookup failed");
            return temporary();
        }
    }

    let token = challenge_token();
    let issued_at = OffsetDateTime::now_utc();
    let expires_at = issued_at + time::Duration::seconds(CHALLENGE_LIFETIME_SECONDS);
    let inserted = sqlx::query(
        r#"
        INSERT INTO area_challenges (
            workspace_id, player_id, drop_id, token_hash, issued_at, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(&payload.drop_id)
    .bind(token_hash(&token))
    .bind(issued_at)
    .bind(expires_at)
    .execute(state.ticketing.pool())
    .await;
    if let Err(error) = inserted {
        tracing::error!(%error, "AREA challenge insert failed");
        return temporary();
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ChallengeResponse {
            ok: true,
            challenge: token,
            issued_at: epoch_millis(issued_at),
            expires_at: epoch_millis(expires_at),
            min_samples: u32::try_from(CHALLENGE_MIN_SAMPLES).unwrap_or(3),
            max_samples: u32::try_from(CHALLENGE_MAX_SAMPLES).unwrap_or(8),
            min_duration_ms: u32::try_from(CHALLENGE_MIN_DURATION_MS).unwrap_or(6_000),
        }),
    )
        .into_response()
}

pub async fn me_challenge(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ChallengeRequest>, JsonRejection>,
) -> Response {
    match mobile_player(&state, &headers).await {
        Ok(Some(player_id)) => issue_challenge(&state, player_id, &headers, payload).await,
        Ok(None) => error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_REQUIRED",
            "Sign in required.",
        ),
        Err(error) => {
            tracing::warn!(%error, "AREA fan session lookup failed");
            temporary()
        }
    }
}

pub async fn internal_challenge(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ChallengeRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    match player_exists(&state, player_id).await {
        Ok(true) => issue_challenge(&state, player_id, &headers, payload).await,
        Ok(false) => error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA player lookup failed");
            temporary()
        }
    }
}

fn valid_samples(samples: &[PositionSample]) -> bool {
    samples.len() >= CHALLENGE_MIN_SAMPLES
        && samples.len() <= CHALLENGE_MAX_SAMPLES
        && samples.iter().all(|sample| {
            sample.lat.is_finite()
                && (-90.0..=90.0).contains(&sample.lat)
                && sample.lng.is_finite()
                && (-180.0..=180.0).contains(&sample.lng)
                && sample.accuracy.is_finite()
                && (0.0..=10_000.0).contains(&sample.accuracy)
                && i64::try_from(sample.captured_at).is_ok()
        })
}

fn to_radians(value: f64) -> f64 {
    value * std::f64::consts::PI / 180.0
}

fn distance_meters(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let earth_radius = 6_371_000.0;
    let delta_lat = to_radians(lat2 - lat1);
    let delta_lng = to_radians(lng2 - lng1);
    let a = (delta_lat / 2.0).sin().powi(2)
        + to_radians(lat1).cos() * to_radians(lat2).cos() * (delta_lng / 2.0).sin().powi(2);
    earth_radius * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        let left = values.get(middle.checked_sub(1)?)?;
        let right = values.get(middle)?;
        Some((left + right) / 2.0)
    } else {
        values.get(middle).copied()
    }
}

fn challenge_time_valid(challenge: &LockedChallenge, samples: &[PositionSample]) -> bool {
    let issued_ms = i64::try_from(epoch_millis(challenge.issued_at)).unwrap_or(i64::MAX);
    let expires_ms = i64::try_from(epoch_millis(challenge.expires_at)).unwrap_or(i64::MIN);
    let now_ms = i64::try_from(epoch_millis(OffsetDateTime::now_utc())).unwrap_or(i64::MAX);
    let mut times = samples
        .iter()
        .filter_map(|sample| i64::try_from(sample.captured_at).ok())
        .collect::<Vec<_>>();
    times.sort_unstable();
    let Some(first) = times.first().copied() else {
        return false;
    };
    let Some(last) = times.last().copied() else {
        return false;
    };
    let upper_bound = now_ms
        .saturating_add(SAMPLE_CLOCK_TOLERANCE_MS)
        .min(expires_ms.saturating_add(SAMPLE_CLOCK_TOLERANCE_MS));
    times.iter().all(|captured| {
        *captured >= issued_ms - SAMPLE_CLOCK_TOLERANCE_MS && *captured <= upper_bound
    }) && last - first >= CHALLENGE_MIN_DURATION_MS
}

fn location_evaluation(drop: &DropRow, samples: &[PositionSample]) -> Result<i32, &'static str> {
    let (Some(exact_lat), Some(exact_lng)) = (drop.exact_lat, drop.exact_lng) else {
        return Err("DROP_INACTIVE");
    };
    let accurate = samples
        .iter()
        .filter(|sample| sample.accuracy <= MAX_ACCURACY_METERS)
        .map(|sample| {
            let distance = distance_meters(sample.lat, sample.lng, exact_lat, exact_lng);
            let tolerance = (sample.accuracy * 0.35).min(15.0);
            (
                distance,
                f64::from(drop.radius_meters) + tolerance,
                sample.accuracy,
            )
        })
        .collect::<Vec<_>>();
    if accurate.len() < CHALLENGE_MIN_SAMPLES {
        return Err("LOW_ACCURACY");
    }
    let inside = accurate
        .iter()
        .filter(|(distance, allowed, _)| distance <= allowed)
        .count();
    let median_distance =
        median(accurate.iter().map(|item| item.0).collect()).ok_or("NOT_ENOUGH_SAMPLES")?;
    let median_accuracy =
        median(accurate.iter().map(|item| item.2).collect()).ok_or("NOT_ENOUGH_SAMPLES")?;
    let allowed = f64::from(drop.radius_meters) + (median_accuracy * 0.35).min(15.0);
    if inside < CHALLENGE_MIN_SAMPLES || median_distance > allowed {
        return Err("OUTSIDE_ZONE");
    }
    let bounded = median_distance.round().clamp(0.0, f64::from(i32::MAX));
    Ok(bounded as i32)
}

async fn lock_drop(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    drop_id: &str,
    player_id: Uuid,
) -> Result<Option<DropRow>, sqlx::Error> {
    let locked = sqlx::query_scalar::<_, String>(
        r#"
        SELECT id
        FROM area_drops
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(drop_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if locked.is_none() {
        return Ok(None);
    }

    sqlx::query_as::<_, DropRow>(
        r#"
        SELECT
            area_drop.id,
            area_drop.number,
            area_drop.city,
            area_drop.region,
            area_drop.signal_city_slug,
            area_drop.map_x,
            area_drop.map_y,
            area_drop.approximate_lat,
            area_drop.approximate_lng,
            area_drop.exact_lat,
            area_drop.exact_lng,
            area_drop.radius_meters,
            area_drop.max_claims,
            area_drop.starts_at,
            area_drop.ends_at,
            area_drop.clue_en,
            area_drop.clue_pl,
            area_drop.collectible_line,
            area_drop.collectible_track,
            area_drop.collectible_edition,
            area_drop.collectible_riddle,
            area_drop.active
                AND area_drop.exact_lat IS NOT NULL
                AND area_drop.exact_lng IS NOT NULL
                AND area_drop.starts_at <= now()
                AND area_drop.ends_at >= now() AS active_now,
            (
                SELECT count(*)::bigint
                FROM area_claims AS claim
                WHERE claim.workspace_id = area_drop.workspace_id
                  AND claim.drop_id = area_drop.id
            ) AS claim_count,
            EXISTS (
                SELECT 1
                FROM area_claims AS player_claim
                WHERE player_claim.workspace_id = area_drop.workspace_id
                  AND player_claim.drop_id = area_drop.id
                  AND player_claim.player_id = $3
            ) AS player_claimed
        FROM area_drops AS area_drop
        WHERE area_drop.workspace_id = $1 AND area_drop.id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(drop_id)
    .bind(player_id)
    .fetch_optional(&mut **transaction)
    .await
}

async fn existing_claim(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    player_id: Uuid,
    drop_id: &str,
) -> Result<Option<ExistingClaim>, sqlx::Error> {
    sqlx::query_as::<_, ExistingClaim>(
        r#"
        SELECT
            claim.drop_id,
            area_drop.number,
            area_drop.city,
            area_drop.collectible_line,
            area_drop.collectible_track,
            area_drop.collectible_edition,
            area_drop.collectible_riddle
        FROM area_claims AS claim
        INNER JOIN area_drops AS area_drop
          ON area_drop.workspace_id = claim.workspace_id
         AND area_drop.id = claim.drop_id
        WHERE claim.workspace_id = $1
          AND claim.player_id = $2
          AND claim.drop_id = $3
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(drop_id)
    .fetch_optional(&mut **transaction)
    .await
}

async fn claim_drop(
    state: &crate::AppState,
    player_id: Uuid,
    headers: &HeaderMap,
    payload: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    if !valid_idempotency_key(headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "A valid Idempotency-Key is required.",
        );
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Invalid request.",
            );
        }
    };
    if !valid_drop_id(&payload.drop_id)
        || payload.challenge.len() < 40
        || payload.challenge.len() > 512
        || !valid_samples(&payload.samples)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Invalid claim data.",
        );
    }

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => {
            tracing::error!(%error, "AREA claim transaction failed to start");
            return temporary();
        }
    };
    match existing_claim(&mut transaction, workspace_id, player_id, &payload.drop_id).await {
        Ok(Some(existing)) => {
            if transaction.commit().await.is_err() {
                return temporary();
            }
            return (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(ClaimResponse {
                    ok: true,
                    already_claimed: true,
                    collectible: Some(collectible_from_existing(&existing)),
                    reward_credits_awarded: 0,
                }),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(%error, "AREA existing claim lookup failed");
            return temporary();
        }
    }

    let challenge = sqlx::query_as::<_, LockedChallenge>(
        r#"
        UPDATE area_challenges
        SET consumed_at = now()
        WHERE workspace_id = $1
          AND player_id = $2
          AND token_hash = $3
          AND consumed_at IS NULL
          AND expires_at > now()
        RETURNING issued_at, expires_at, drop_id
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(token_hash(&payload.challenge))
    .fetch_optional(&mut *transaction)
    .await;
    let challenge = match challenge {
        Ok(Some(challenge)) if challenge.drop_id == payload.drop_id => challenge,
        Ok(_) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "CHALLENGE_INVALID",
                "The location challenge is invalid or expired.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "AREA challenge consumption failed");
            return temporary();
        }
    };
    if !challenge_time_valid(&challenge, &payload.samples) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "NOT_ENOUGH_SAMPLES",
            "Not enough fresh location samples.",
        );
    }

    let drop = match lock_drop(&mut transaction, workspace_id, &payload.drop_id, player_id).await {
        Ok(Some(drop)) => drop,
        Ok(None) => {
            return error_response(
                StatusCode::CONFLICT,
                "DROP_INACTIVE",
                "This drop is not active.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "AREA drop lock failed");
            return temporary();
        }
    };
    if !drop.active_now {
        return error_response(
            StatusCode::CONFLICT,
            "DROP_INACTIVE",
            "This drop is not active.",
        );
    }
    if drop.claim_count >= i64::from(drop.max_claims) {
        return error_response(
            StatusCode::CONFLICT,
            "DROP_FULL",
            "This drop has reached its claim limit.",
        );
    }
    let distance = match location_evaluation(&drop, &payload.samples) {
        Ok(distance) => distance,
        Err("DROP_INACTIVE") => {
            return error_response(
                StatusCode::CONFLICT,
                "DROP_INACTIVE",
                "This drop is not configured.",
            );
        }
        Err("LOW_ACCURACY") => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "LOW_ACCURACY",
                "Location is not accurate enough. Move outdoors and retry.",
            );
        }
        Err("OUTSIDE_ZONE") => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "OUTSIDE_ZONE",
                "You are outside the drop zone.",
            );
        }
        Err(_) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "NOT_ENOUGH_SAMPLES",
                "Not enough location samples.",
            );
        }
    };
    let edition_number = match next_edition_number(
        &mut transaction,
        workspace_id,
        &payload.drop_id,
        drop.max_claims,
        None,
    )
    .await
    {
        Ok(Some(number)) => number,
        Ok(None) => {
            return error_response(
                StatusCode::CONFLICT,
                "DROP_FULL",
                "This drop has reached its claim limit.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "AREA edition allocation failed");
            return temporary();
        }
    };
    let inserted = sqlx::query(
        r#"
        INSERT INTO area_claims (
            workspace_id, player_id, drop_id, distance_meters,
            edition_number, claim_source
        )
        VALUES ($1, $2, $3, $4, $5, 'gps')
        ON CONFLICT (workspace_id, player_id, drop_id) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(&payload.drop_id)
    .bind(distance)
    .bind(edition_number)
    .execute(&mut *transaction)
    .await;
    let inserted = match inserted {
        Ok(result) => result.rows_affected() == 1,
        Err(error) => {
            tracing::error!(%error, "AREA claim insert failed");
            return temporary();
        }
    };
    if !inserted {
        match existing_claim(&mut transaction, workspace_id, player_id, &payload.drop_id).await {
            Ok(Some(existing)) => {
                if let Err(error) = transaction.commit().await {
                    tracing::error!(%error, "AREA concurrent claim commit failed");
                    return temporary();
                }
                return (
                    StatusCode::OK,
                    [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                    Json(ClaimResponse {
                        ok: true,
                        already_claimed: true,
                        collectible: Some(collectible_from_existing(&existing)),
                        reward_credits_awarded: 0,
                    }),
                )
                    .into_response();
            }
            Ok(None) => {
                return error_response(
                    StatusCode::CONFLICT,
                    "CLAIM_CONFLICT",
                    "The claim was already processed. Refresh progress.",
                );
            }
            Err(error) => {
                tracing::error!(%error, "AREA concurrent claim reload failed");
                return temporary();
            }
        }
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "AREA claim commit failed");
        return temporary();
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ClaimResponse {
            ok: true,
            already_claimed: false,
            collectible: Some(AreaCollectible {
                drop_id: drop.id,
                number: drop.number,
                city: drop.city,
                line: drop.collectible_line,
                track: drop.collectible_track,
                edition: drop.collectible_edition,
                riddle: drop.collectible_riddle,
            }),
            reward_credits_awarded: 1,
        }),
    )
        .into_response()
}

pub async fn me_claim(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    match mobile_player(&state, &headers).await {
        Ok(Some(player_id)) => claim_drop(&state, player_id, &headers, payload).await,
        Ok(None) => error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_REQUIRED",
            "Sign in required.",
        ),
        Err(error) => {
            tracing::warn!(%error, "AREA fan session lookup failed");
            temporary()
        }
    }
}

pub async fn internal_claim(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    match player_exists(&state, player_id).await {
        Ok(true) => claim_drop(&state, player_id, &headers, payload).await,
        Ok(false) => error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA player lookup failed");
            temporary()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_drop_ids() {
        assert!(valid_drop_id("wro-001"));
        assert!(valid_drop_id("tor-012"));
        assert!(!valid_drop_id("WRO-001"));
        assert!(!valid_drop_id("wro-01"));
    }

    #[test]
    fn idempotency_key_must_be_a_uuid() -> Result<(), axum::http::header::InvalidHeaderValue> {
        let mut headers = HeaderMap::new();
        assert!(!valid_idempotency_key(&headers));
        headers.insert(IDEMPOTENCY_KEY.clone(), "not-a-uuid".parse()?);
        assert!(!valid_idempotency_key(&headers));
        headers.insert(IDEMPOTENCY_KEY.clone(), Uuid::now_v7().to_string().parse()?);
        assert!(valid_idempotency_key(&headers));
        Ok(())
    }

    #[test]
    fn distance_is_zero_for_same_position() {
        assert!(distance_meters(51.0, 17.0, 51.0, 17.0) < 0.01);
    }

    #[test]
    fn median_handles_even_and_odd_samples() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(Vec::new()), None);
    }
}
