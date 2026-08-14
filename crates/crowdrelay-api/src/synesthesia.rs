//! Public Synesthesia completion ledger.
//!
//! The game remains offline-first. This module only records a bounded,
//! pseudonymous completion trail and optional draw entry. It deliberately does
//! not collect shipping data and does not alter the existing fan mail flow.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{AUTHORIZATION, CACHE_CONTROL},
    },
    response::{IntoResponse, Response},
};
use crowdrelay_domain::NormalizedEmail;
use getrandom::fill as fill_random;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{Problem, acquisition::fan_session_from_headers, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const CAMPAIGN_SLUG: &str = "virya-synesthesia-album-v1";
const ROOM_IDS: [&str; 11] = [
    "wave-of-uncertainty",
    "party-time",
    "unmasked",
    "the-calling",
    "seed-of-doubt",
    "hybrid",
    "technophobia",
    "invaluable",
    "from-the-ashes",
    "waves",
    "rise",
];
const MIN_ROOM_ELAPSED_MS: i64 = 1_000;
const MAX_ROOM_ELAPSED_MS: i64 = 7_200_000;
const MAX_TOTAL_ELAPSED_MS: i64 = 24 * 60 * 60 * 1000;
const HANDOFF_TTL_MINUTES: i64 = 15;
const LEADERBOARD_DEFAULT_LIMIT: u16 = 10;
const LEADERBOARD_MAX_LIMIT: u16 = 50;
const PUBLIC_LEADERBOARD_CACHE: &str = "public, max-age=15, s-maxage=30, stale-while-revalidate=60";

mod leaderboard;
pub use leaderboard::{list_leaderboard, publish_leaderboard};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRunRequest {
    campaign_slug: String,
    install_id: String,
    app_version: String,
    attempt_id: Option<String>,
    locale: Option<String>,
}

#[derive(Debug, Serialize)]
struct StartRunResponse {
    run_id: Uuid,
    run_token: String,
    next_room_index: i16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRoomRequest {
    room_index: i16,
    client_elapsed_ms: i64,
}

#[derive(Debug, Serialize)]
struct RecordRoomResponse {
    next_room_index: i16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteRunRequest {
    client_total_elapsed_ms: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverRunRequest {
    completed_room_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CompleteRunResponse {
    completed: bool,
    linked_to_fan: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    handoff_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "time::serde::rfc3339::option")]
    handoff_expires_at: Option<time::OffsetDateTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_event: Option<SynesthesiaNextEvent>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct SynesthesiaNextEvent {
    slug: String,
    title: String,
    venue: Option<String>,
    city: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: time::OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRunRequest {
    handoff_code: String,
}

#[derive(Debug, Serialize)]
struct LinkRunResponse {
    linked: bool,
    run_id: Uuid,
    rooms_completed: i16,
    client_total_elapsed_ms: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewardEntryRequest {
    email: String,
    policy_version: String,
    locale: Option<String>,
}

#[derive(Debug, Serialize)]
struct RewardEntryResponse {
    status: &'static str,
    message: &'static str,
    draw_size: u8,
}

#[derive(Clone, Copy, Debug)]
enum SynesthesiaError {
    Invalid,
    Unauthorized,
    Conflict,
    Unavailable,
}

impl SynesthesiaError {
    fn response(self, request_id_value: Option<String>) -> Response {
        match self {
            Self::Invalid => Problem::unprocessable(request_id_value)
                .private()
                .into_response(),
            Self::Unauthorized => Problem::unauthorized(request_id_value)
                .private()
                .into_response(),
            Self::Conflict => Problem::conflict(request_id_value)
                .private()
                .into_response(),
            Self::Unavailable => Problem::service_unavailable(request_id_value)
                .private()
                .into_response(),
        }
    }

    fn sqlx(error: sqlx::Error) -> Self {
        tracing::warn!(%error, "synesthesia persistence failed");
        Self::Unavailable
    }
}

include!("synesthesia/run_lifecycle.rs");
include!("synesthesia/rewards.rs");
include!("synesthesia/validation.rs");
