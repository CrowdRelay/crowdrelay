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
use time::{Duration, OffsetDateTime};
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

#[derive(Clone, Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AreaVoucherPublic {
    request_id: Uuid,
    code: String,
    tokens: i32,
    benefit: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    expires_at: i64,
    status: String,
    free_product_id: Option<String>,
    free_product_label: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    redeemed_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AreaTicketRewardPublic {
    request_id: Uuid,
    event_slug: String,
    credits: i32,
    status: String,
    public_reference: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    issued_at: Option<OffsetDateTime>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateVoucherRequest {
    request_id: Uuid,
    #[serde(default = "one_credit")]
    tokens: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RewardCodeRequest {
    code: String,
    #[serde(default)]
    reservation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReserveRewardRequest {
    code: String,
    reservation_id: String,
    reserved_until: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachRewardCheckoutRequest {
    code_hash: String,
    reservation_id: String,
    checkout_session_id: String,
    free_product_id: String,
    free_product_label: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReconcileRewardRequest {
    code_hash: String,
    reservation_id: String,
    checkout_session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseRewardRequest {
    code_hash: String,
    reservation_id: String,
    #[serde(default)]
    checkout_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyVoucherImport {
    request_id: Uuid,
    code: String,
    tokens: u32,
    benefit: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    expires_at: i64,
    status: String,
    #[serde(default)]
    reservation_id: Option<String>,
    #[serde(default)]
    reserved_until: Option<i64>,
    #[serde(default)]
    checkout_session_id: Option<String>,
    #[serde(default)]
    free_product_id: Option<String>,
    #[serde(default)]
    free_product_label: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    redeemed_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyTicketRewardImport {
    request_id: Uuid,
    event_slug: String,
    credits: u32,
    fan_email: String,
    #[serde(default)]
    public_reference: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    issued_at: Option<OffsetDateTime>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportLegacyWalletRequest {
    migration_id: String,
    token_balance: u32,
    #[serde(default)]
    vouchers: Vec<LegacyVoucherImport>,
    #[serde(default)]
    ticket_rewards: Vec<LegacyTicketRewardImport>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReserveTicketRewardRequest {
    request_id: Uuid,
    event_slug: String,
    credits: u32,
    fan_email: String,
    reservation_id: String,
    reservation_expires_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FinalizeTicketRewardRequest {
    request_id: Uuid,
    reservation_id: String,
    public_reference: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailTicketRewardRequest {
    request_id: Uuid,
    reservation_id: String,
    permanent: bool,
    #[serde(default)]
    failure_code: Option<String>,
}

fn one_credit() -> u32 {
    1
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AreaWallet {
    authenticated: bool,
    migration_required: bool,
    legacy_migration_applied: bool,
    token_balance: u32,
    reward_credits: u32,
    reward: RewardSummary,
    collection_size: u32,
    community: AreaCommunity,
    claims: Vec<AreaClaim>,
    vouchers: Vec<AreaVoucherPublic>,
    ticket_rewards: Vec<AreaTicketRewardPublic>,
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

// Physical sections compile into this module through `include!`.
// This preserves the established API and item visibility while keeping
// high-risk domains small enough to review and profile independently.
include!("area/ledger.rs");
include!("area/storage.rs");
include!("area/endpoints.rs");
include!("area/challenge.rs");
include!("area/claims.rs");
include!("area/rewards.rs");
include!("area/ticket_rewards.rs");
include!("area/legacy_wallet.rs");
include!("area/tests.rs");
