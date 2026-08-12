//! Public Synesthesia completion ledger.
//!
//! The game remains offline-first. This module only records a bounded,
//! pseudonymous completion trail and optional draw entry. It deliberately does
//! not collect shipping data and does not alter the existing fan mail flow.

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaderboardQuery {
    limit: Option<u16>,
}

#[derive(Debug, Serialize)]
struct LeaderboardEntryResponse {
    rank: u16,
    display_name: String,
    elapsed_ms: i64,
}

#[derive(Debug, Serialize)]
struct LeaderboardResponse {
    items: Vec<LeaderboardEntryResponse>,
}

#[derive(Debug, Serialize)]
struct LeaderboardPublishResponse {
    published: bool,
    display_name: String,
    rank: i64,
    best_elapsed_ms: i64,
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

pub async fn start_run(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<StartRunRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    if validate_start(&payload).is_err() {
        return SynesthesiaError::Invalid.response(request_id_value);
    }

    let token = match random_token() {
        Ok(token) => token,
        Err(()) => return SynesthesiaError::Unavailable.response(request_id_value),
    };
    let token_hash = Sha256::digest(token.as_bytes()).to_vec();
    let install_hash =
        Sha256::digest(format!("{}\0{}", payload.campaign_slug, payload.install_id).as_bytes())
            .to_vec();
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let row = sqlx::query_as::<_, (Uuid, i16)>(
        r#"
        INSERT INTO synesthesia_runs (
            workspace_id, campaign_slug, install_hash, run_token_hash,
            app_version, attempt_id, locale
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (workspace_id, campaign_slug, install_hash, attempt_id) DO UPDATE
        SET run_token_hash = EXCLUDED.run_token_hash,
            app_version = EXCLUDED.app_version,
            locale = EXCLUDED.locale,
            updated_at = now()
        RETURNING id, next_room_index
        "#,
    )
    .bind(workspace_id)
    .bind(&payload.campaign_slug)
    .bind(install_hash)
    .bind(token_hash)
    .bind(payload.app_version.trim())
    .bind(clean_attempt_id(payload.attempt_id.as_deref()).unwrap_or_else(|| "legacy".to_owned()))
    .bind(clean_locale(payload.locale.as_deref()))
    .fetch_one(state.ticketing.pool())
    .await;

    match row {
        Ok((run_id, next_room_index)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(StartRunResponse {
                run_id,
                run_token: token,
                next_room_index,
            }),
        )
            .into_response(),
        Err(error) => SynesthesiaError::sqlx(error).response(request_id_value),
    }
}

pub async fn record_room(
    State(state): State<crate::AppState>,
    Path((run_id, room_id)): Path<(Uuid, String)>,
    headers: HeaderMap,
    payload: Result<Json<RecordRoomRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    if payload.room_index < 0
        || usize::try_from(payload.room_index)
            .ok()
            .is_none_or(|index| index >= ROOM_IDS.len())
        || !(MIN_ROOM_ELAPSED_MS..=MAX_ROOM_ELAPSED_MS).contains(&payload.client_elapsed_ms)
    {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };

    let result = record_room_inner(
        &mut transaction,
        workspace_id,
        run_id,
        &room_id,
        &token_hash,
        &payload,
    )
    .await;
    let next_room_index = match result {
        Ok(index) => index,
        Err(error) => return error.response(request_id_value),
    };
    if let Err(error) = transaction.commit().await {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }
    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(RecordRoomResponse { next_room_index }),
    )
        .into_response()
}

async fn record_room_inner(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    run_id: Uuid,
    room_id: &str,
    token_hash: &[u8],
    payload: &RecordRoomRequest,
) -> Result<i16, SynesthesiaError> {
    let Some((campaign_slug, next_room_index, completed_at)) =
        sqlx::query_as::<_, (String, i16, Option<time::OffsetDateTime>)>(
            r#"
            SELECT campaign_slug, next_room_index, completed_at
            FROM synesthesia_runs
            WHERE workspace_id = $1 AND id = $2 AND run_token_hash = $3
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(token_hash)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(SynesthesiaError::sqlx)?
    else {
        return Err(SynesthesiaError::Unauthorized);
    };
    if campaign_slug != CAMPAIGN_SLUG || completed_at.is_some() {
        return Err(SynesthesiaError::Conflict);
    }

    if payload.room_index < next_room_index {
        let exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM synesthesia_room_completions
                WHERE workspace_id = $1 AND run_id = $2
                  AND room_index = $3 AND room_id = $4
            )
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(payload.room_index)
        .bind(room_id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(SynesthesiaError::sqlx)?;
        return if exists {
            Ok(next_room_index)
        } else {
            Err(SynesthesiaError::Conflict)
        };
    }
    if payload.room_index != next_room_index {
        return Err(SynesthesiaError::Conflict);
    }
    let expected_room = usize::try_from(next_room_index)
        .ok()
        .and_then(|index| ROOM_IDS.get(index))
        .copied()
        .ok_or(SynesthesiaError::Conflict)?;
    if room_id != expected_room {
        return Err(SynesthesiaError::Conflict);
    }

    sqlx::query(
        r#"
        INSERT INTO synesthesia_room_completions (
            workspace_id, run_id, room_index, room_id, client_elapsed_ms
        ) VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workspace_id, run_id, room_index) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(payload.room_index)
    .bind(room_id)
    .bind(payload.client_elapsed_ms)
    .execute(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;

    let advanced = next_room_index.saturating_add(1);
    sqlx::query(
        r#"
        UPDATE synesthesia_runs
        SET next_room_index = $3, updated_at = now()
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(advanced)
    .execute(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;
    Ok(advanced)
}

pub async fn complete_run(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<CompleteRunRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    if !(i64::try_from(ROOM_IDS.len()).unwrap_or(11) * MIN_ROOM_ELAPSED_MS..=MAX_TOTAL_ELAPSED_MS)
        .contains(&payload.client_total_elapsed_ms)
    {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let row = sqlx::query_as::<_, (i16, Option<time::OffsetDateTime>, i64)>(
        r#"
        SELECT run.next_room_index, run.completed_at,
               COALESCE(SUM(room.client_elapsed_ms), 0)::bigint AS recorded_elapsed_ms
        FROM synesthesia_runs AS run
        LEFT JOIN synesthesia_room_completions AS room
          ON room.workspace_id = run.workspace_id AND room.run_id = run.id
        WHERE run.workspace_id = $1 AND run.id = $2 AND run.run_token_hash = $3
        GROUP BY run.id
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(token_hash)
    .fetch_optional(state.ticketing.pool())
    .await;
    let Some((next_room_index, completed_at, recorded_elapsed_ms)) = (match row {
        Ok(row) => row,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    }) else {
        return SynesthesiaError::Unauthorized.response(request_id_value);
    };
    if completed_at.is_none() {
        if usize::try_from(next_room_index).ok() != Some(ROOM_IDS.len())
            || payload.client_total_elapsed_ms != recorded_elapsed_ms
        {
            return SynesthesiaError::Conflict.response(request_id_value);
        }
        let result = sqlx::query(
            r#"
            UPDATE synesthesia_runs
            SET completed_at = now(), client_total_elapsed_ms = $3, updated_at = now()
            WHERE workspace_id = $1 AND id = $2 AND completed_at IS NULL
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(payload.client_total_elapsed_ms)
        .execute(state.ticketing.pool())
        .await;
        if let Err(error) = result {
            return SynesthesiaError::sqlx(error).response(request_id_value);
        }
    }

    match completion_response(&state, workspace_id, run_id).await {
        Ok(response) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(response),
        )
            .into_response(),
        Err(error) => error.response(request_id_value),
    }
}

async fn completion_response(
    state: &crate::AppState,
    workspace_id: Uuid,
    run_id: Uuid,
) -> Result<CompleteRunResponse, SynesthesiaError> {
    let linked_to_fan = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT fan_id IS NOT NULL
        FROM synesthesia_runs
        WHERE workspace_id = $1 AND id = $2 AND completed_at IS NOT NULL
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(SynesthesiaError::sqlx)?
    .ok_or(SynesthesiaError::Conflict)?;

    let (handoff_code, handoff_expires_at) = if linked_to_fan {
        (None, None)
    } else {
        let code = random_token().map_err(|()| SynesthesiaError::Unavailable)?;
        let hash = Sha256::digest(code.as_bytes()).to_vec();
        let expires_at =
            time::OffsetDateTime::now_utc() + time::Duration::minutes(HANDOFF_TTL_MINUTES);
        sqlx::query(
            r#"
            UPDATE synesthesia_runs
            SET handoff_token_hash = $3, handoff_expires_at = $4, updated_at = now()
            WHERE workspace_id = $1 AND id = $2 AND fan_id IS NULL AND completed_at IS NOT NULL
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(hash)
        .bind(expires_at)
        .execute(state.ticketing.pool())
        .await
        .map_err(SynesthesiaError::sqlx)?;
        (Some(code), Some(expires_at))
    };

    let next_event = sqlx::query_as::<_, SynesthesiaNextEvent>(
        r#"
        SELECT event.slug, event.title, event.venue, city.name AS city, event.starts_at
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1
          AND event.status = 'published'
          AND event.starts_at > now()
        ORDER BY event.starts_at, event.id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(SynesthesiaError::sqlx)?;

    Ok(CompleteRunResponse {
        completed: true,
        linked_to_fan,
        handoff_code,
        handoff_expires_at,
        next_event,
    })
}

pub async fn link_completed_run_to_fan(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<LinkRunRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    let handoff_code = payload.handoff_code.trim().to_ascii_lowercase();
    if handoff_code.len() != 64 || !handoff_code.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let Some(session) = fan_session_from_headers(&headers) else {
        return SynesthesiaError::Unauthorized.response(request_id_value);
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };
    let fan_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        UPDATE fan_sessions AS session
        SET last_seen_at = now()
        FROM fans AS fan
        WHERE session.workspace_id = $1
          AND session.session_token_hash = digest($2, 'sha256')
          AND session.revoked_at IS NULL
          AND session.expires_at > now()
          AND fan.workspace_id = session.workspace_id
          AND fan.id = session.fan_id
          AND fan.status = 'active'
        RETURNING session.fan_id
        "#,
    )
    .bind(workspace_id)
    .bind(session.as_str())
    .fetch_optional(&mut *transaction)
    .await;
    let fan_id = match fan_id {
        Ok(Some(fan_id)) => fan_id,
        Ok(None) => return SynesthesiaError::Unauthorized.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };

    let handoff_hash = Sha256::digest(handoff_code.as_bytes()).to_vec();
    let linked = sqlx::query_as::<_, (Uuid, i16, i64)>(
        r#"
        UPDATE synesthesia_runs
        SET fan_id = $3, linked_at = COALESCE(linked_at, now()), updated_at = now()
        WHERE workspace_id = $1
          AND handoff_token_hash = $2
          AND handoff_expires_at > now()
          AND completed_at IS NOT NULL
          AND (fan_id IS NULL OR fan_id = $3)
        RETURNING id, next_room_index, COALESCE(client_total_elapsed_ms, 0)
        "#,
    )
    .bind(workspace_id)
    .bind(handoff_hash)
    .bind(fan_id)
    .fetch_optional(&mut *transaction)
    .await;
    let (run_id, rooms_completed, client_total_elapsed_ms) = match linked {
        Ok(Some(value)) => value,
        Ok(None) => return SynesthesiaError::Conflict.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };
    if let Err(error) = transaction.commit().await {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }
    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(LinkRunResponse {
            linked: true,
            run_id,
            rooms_completed,
            client_total_elapsed_ms,
        }),
    )
        .into_response()
}

pub async fn list_leaderboard(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Result<Query<LeaderboardQuery>, QueryRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    let limit = query.limit.unwrap_or(LEADERBOARD_DEFAULT_LIMIT);
    if limit == 0 || limit > LEADERBOARD_MAX_LIMIT {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let rows = sqlx::query_as::<_, (String, i64)>(
        r#"
        WITH best AS (
            SELECT DISTINCT ON (run.fan_id)
                run.leaderboard_name AS display_name,
                run.client_total_elapsed_ms AS elapsed_ms,
                run.completed_at,
                run.id
            FROM synesthesia_runs AS run
            WHERE run.workspace_id = $1
              AND run.campaign_slug = $2
              AND run.fan_id IS NOT NULL
              AND run.completed_at IS NOT NULL
              AND run.client_total_elapsed_ms IS NOT NULL
              AND run.leaderboard_name IS NOT NULL
            ORDER BY run.fan_id, run.client_total_elapsed_ms, run.completed_at, run.id
        )
        SELECT display_name, elapsed_ms
        FROM best
        ORDER BY elapsed_ms, completed_at, id
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(CAMPAIGN_SLUG)
    .bind(i64::from(limit))
    .fetch_all(state.ticketing.pool())
    .await;

    match rows {
        Ok(rows) => {
            let items = rows
                .into_iter()
                .enumerate()
                .map(
                    |(index, (display_name, elapsed_ms))| LeaderboardEntryResponse {
                        rank: u16::try_from(index + 1).unwrap_or(u16::MAX),
                        display_name,
                        elapsed_ms,
                    },
                )
                .collect();
            (
                StatusCode::OK,
                [(
                    CACHE_CONTROL,
                    HeaderValue::from_static(PUBLIC_LEADERBOARD_CACHE),
                )],
                Json(LeaderboardResponse { items }),
            )
                .into_response()
        }
        Err(error) => SynesthesiaError::sqlx(error).response(request_id_value),
    }
}

pub async fn publish_leaderboard(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };

    let authorized = sqlx::query_as::<_, (String, Uuid, String)>(
        r#"
        SELECT run.campaign_slug, run.fan_id, fan.normalized_email
        FROM synesthesia_runs AS run
        INNER JOIN fans AS fan
          ON fan.workspace_id = run.workspace_id AND fan.id = run.fan_id
        WHERE run.workspace_id = $1 AND run.id = $2 AND run.run_token_hash = $3
          AND run.completed_at IS NOT NULL
        FOR SHARE OF run, fan
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(&token_hash)
    .fetch_optional(&mut *transaction)
    .await;
    let (campaign_slug, fan_id, normalized_email) = match authorized {
        Ok(Some(value)) => value,
        Ok(None) => return SynesthesiaError::Conflict.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };
    if campaign_slug != CAMPAIGN_SLUG {
        return SynesthesiaError::Conflict.response(request_id_value);
    }
    let display_name = match masked_email_alias(&normalized_email) {
        Some(alias) => alias,
        None => return SynesthesiaError::Conflict.response(request_id_value),
    };

    if let Err(error) = sqlx::query(
        r#"
        UPDATE synesthesia_runs
        SET leaderboard_name = $4,
            leaderboard_published_at = COALESCE(leaderboard_published_at, now()),
            updated_at = now()
        WHERE workspace_id = $1 AND campaign_slug = $2 AND fan_id = $3
          AND completed_at IS NOT NULL AND client_total_elapsed_ms IS NOT NULL
        "#,
    )
    .bind(workspace_id)
    .bind(CAMPAIGN_SLUG)
    .bind(fan_id)
    .bind(&display_name)
    .execute(&mut *transaction)
    .await
    {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }

    let ranked = sqlx::query_as::<_, (i64, i64)>(
        r#"
        WITH best AS (
            SELECT DISTINCT ON (run.fan_id)
                run.fan_id,
                run.client_total_elapsed_ms AS elapsed_ms,
                run.completed_at,
                run.id
            FROM synesthesia_runs AS run
            WHERE run.workspace_id = $1
              AND run.campaign_slug = $2
              AND run.fan_id IS NOT NULL
              AND run.completed_at IS NOT NULL
              AND run.client_total_elapsed_ms IS NOT NULL
              AND run.leaderboard_name IS NOT NULL
            ORDER BY run.fan_id, run.client_total_elapsed_ms, run.completed_at, run.id
        ), ranked AS (
            SELECT fan_id, elapsed_ms,
                   ROW_NUMBER() OVER (ORDER BY elapsed_ms, completed_at, id)::bigint AS rank
            FROM best
        )
        SELECT rank, elapsed_ms FROM ranked WHERE fan_id = $3
        "#,
    )
    .bind(workspace_id)
    .bind(CAMPAIGN_SLUG)
    .bind(fan_id)
    .fetch_optional(&mut *transaction)
    .await;
    let (rank, best_elapsed_ms) = match ranked {
        Ok(Some(value)) => value,
        Ok(None) => return SynesthesiaError::Conflict.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };
    if let Err(error) = transaction.commit().await {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(LeaderboardPublishResponse {
            published: true,
            display_name,
            rank,
            best_elapsed_ms,
        }),
    )
        .into_response()
}

pub async fn enter_reward_draw(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RewardEntryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    let email = match NormalizedEmail::parse(&payload.email) {
        Ok(email) => email,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    if payload.policy_version.trim().is_empty()
        || payload.policy_version.len() > 120
        || clean_locale(payload.locale.as_deref()).is_none() && payload.locale.is_some()
    {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };

    let result = enter_reward_draw_inner(
        &mut transaction,
        workspace_id,
        &token_hash,
        email.as_str(),
        &payload,
    )
    .await;
    match result {
        Ok(()) => {}
        Err(error) => return error.response(request_id_value),
    }
    if let Err(error) = transaction.commit().await {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(RewardEntryResponse {
            status: "entered_draw",
            message: "Jesteś w losowaniu jednej z 5 płyt Echoes Of The Modern Mind. Jedno ukończenie = jeden los.",
            draw_size: 5,
        }),
    )
        .into_response()
}

async fn enter_reward_draw_inner(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    token_hash: &[u8],
    normalized_email: &str,
    payload: &RewardEntryRequest,
) -> Result<(), SynesthesiaError> {
    let Some((run_id, campaign_slug)) = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, campaign_slug
        FROM synesthesia_runs
        WHERE workspace_id = $1 AND run_token_hash = $2 AND completed_at IS NOT NULL
        FOR SHARE
        "#,
    )
    .bind(workspace_id)
    .bind(token_hash)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?
    else {
        return Err(SynesthesiaError::Unauthorized);
    };
    if campaign_slug != CAMPAIGN_SLUG {
        return Err(SynesthesiaError::Conflict);
    }

    let draw_is_open = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM reward_draws
            WHERE workspace_id = $1
              AND eligibility_kind = 'synesthesia_completion'
              AND eligibility_ref = $2
              AND status = 'scheduled'
              AND opens_at <= now()
              AND closes_at > now()
        )
        "#,
    )
    .bind(workspace_id)
    .bind(&campaign_slug)
    .fetch_one(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;
    if !draw_is_open {
        return Err(SynesthesiaError::Conflict);
    }

    let fan_id = match sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, status
        FROM fans
        WHERE workspace_id = $1 AND normalized_email = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(normalized_email)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?
    {
        Some((_, status)) if status == "suppressed" => return Err(SynesthesiaError::Conflict),
        Some((fan_id, _)) => fan_id,
        None => sqlx::query_scalar::<_, Uuid>(
            r#"
                INSERT INTO fans (workspace_id, normalized_email, locale, status)
                VALUES ($1, $2, $3, 'pending')
                RETURNING id
                "#,
        )
        .bind(workspace_id)
        .bind(normalized_email)
        .bind(clean_locale(payload.locale.as_deref()))
        .fetch_one(&mut **transaction)
        .await
        .map_err(SynesthesiaError::sqlx)?,
    };

    let linked_run = sqlx::query(
        r#"
        UPDATE synesthesia_runs
        SET fan_id = $3, linked_at = COALESCE(linked_at, now()),
            handoff_token_hash = NULL, handoff_expires_at = NULL, updated_at = now()
        WHERE workspace_id = $1 AND id = $2
          AND (fan_id IS NULL OR fan_id = $3)
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(fan_id)
    .execute(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;
    if linked_run.rows_affected() != 1 {
        return Err(SynesthesiaError::Conflict);
    }

    sqlx::query(
        r#"
        INSERT INTO synesthesia_reward_entries (
            workspace_id, campaign_slug, run_id, fan_id, normalized_email,
            policy_version, locale
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (workspace_id, campaign_slug, normalized_email) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(&campaign_slug)
    .bind(run_id)
    .bind(fan_id)
    .bind(normalized_email)
    .bind(payload.policy_version.trim())
    .bind(clean_locale(payload.locale.as_deref()))
    .execute(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;

    Ok(())
}

fn validate_start(payload: &StartRunRequest) -> Result<(), ()> {
    if payload.campaign_slug != CAMPAIGN_SLUG
        || payload.install_id.len() < 24
        || payload.install_id.len() > 128
        || !payload
            .install_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        || payload.app_version.trim().is_empty()
        || payload.app_version.len() > 64
        || (payload.attempt_id.is_some()
            && clean_attempt_id(payload.attempt_id.as_deref()).is_none())
        || (payload.locale.is_some() && clean_locale(payload.locale.as_deref()).is_none())
    {
        return Err(());
    }
    Ok(())
}

fn clean_attempt_id(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(value.to_owned())
}

fn masked_email_alias(value: &str) -> Option<String> {
    let (local, domain) = value.rsplit_once('@')?;
    if local.is_empty() || domain.is_empty() {
        return None;
    }
    let local_prefix: String = local.chars().take(3).collect();
    (!local_prefix.is_empty()).then(|| format!("{local_prefix}••••"))
}

fn clean_locale(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty()
        || value.len() > 35
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(value.to_owned())
}

fn bearer_hash(headers: &HeaderMap) -> Option<Vec<u8>> {
    let token = headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?
        .trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Sha256::digest(token.to_ascii_lowercase().as_bytes()).to_vec())
}

fn random_token() -> Result<String, ()> {
    let mut bytes = [0_u8; 32];
    fill_random(&mut bytes).map_err(|_| ())?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_contract_is_fixed_and_ordered() {
        assert_eq!(ROOM_IDS.len(), 11);
        assert_eq!(ROOM_IDS.first(), Some(&"wave-of-uncertainty"));
        assert_eq!(ROOM_IDS.last(), Some(&"rise"));
    }

    #[test]
    fn public_identifiers_are_bounded() {
        assert!(clean_locale(Some("pl-PL")).is_some());
        assert!(clean_locale(Some("pl/PL")).is_none());
        assert_eq!(
            clean_attempt_id(Some("attempt_01-A")),
            Some("attempt_01-A".to_owned())
        );
        assert!(clean_attempt_id(Some("bad/attempt")).is_none());
    }

    #[test]
    fn leaderboard_aliases_mask_identity_and_stay_bounded() {
        assert_eq!(
            masked_email_alias("wojciech@example.com"),
            Some("woj••••".to_owned())
        );
        assert_eq!(masked_email_alias("a@b.pl"), Some("a••••".to_owned()));
        assert!(masked_email_alias("not-an-email").is_none());
        assert!(masked_email_alias("@example.com").is_none());
    }
}
