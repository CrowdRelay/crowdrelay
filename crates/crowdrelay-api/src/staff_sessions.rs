//! One-time staff pairing and revocable per-device operator sessions.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use getrandom::fill;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const DEFAULT_PAIRING_TTL_MINUTES: i64 = 5;
const MAX_PAIRING_TTL_MINUTES: i64 = 10;
const DEVICE_SESSION_TTL_DAYS: i64 = 7;
const MAX_ACTIVE_DEVICE_SESSIONS: i64 = 32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CreatePairingCodeRequest {
    display_name: String,
    #[serde(default = "default_pairing_ttl_minutes")]
    ttl_minutes: i64,
}

fn default_pairing_ttl_minutes() -> i64 {
    DEFAULT_PAIRING_TTL_MINUTES
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingCodeResponse {
    version: u8,
    role: &'static str,
    display_name: String,
    pairing_code: String,
    expires_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExchangePairingCodeRequest {
    pairing_code: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSessionResponse {
    version: u8,
    role: &'static str,
    display_name: String,
    bearer_token: String,
    session_id: Uuid,
    expires_at: i64,
}

#[derive(Debug, FromRow)]
struct PairingCodeRow {
    id: Uuid,
    display_name: String,
    expires_at: OffsetDateTime,
}

#[derive(Debug, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSessionView {
    id: Uuid,
    display_name: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    revoked_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSessionListResponse {
    sessions: Vec<DeviceSessionView>,
}

fn clean_display_name(value: &str) -> Option<String> {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars = cleaned.chars().count();
    if !(1..=64).contains(&chars) || cleaned.chars().any(char::is_control) {
        return None;
    }
    Some(cleaned)
}

fn clean_pairing_code(value: &str) -> Option<&str> {
    let value = value.trim();
    if !(24..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(value)
}

fn random_token<const N: usize>() -> Option<String> {
    let mut bytes = [0_u8; N];
    fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn token_hash(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

pub async fn create_pairing_code(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreatePairingCodeRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(display_name) = clean_display_name(&payload.display_name) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    if !(1..=MAX_PAIRING_TTL_MINUTES).contains(&payload.ttl_minutes) {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }
    let Some(pairing_code) = random_token::<20>() else {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };
    let now = OffsetDateTime::now_utc();
    let expires_at = now + Duration::minutes(payload.ttl_minutes);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let result = sqlx::query(
        r#"
        INSERT INTO staff_pairing_codes (workspace_id, id, code_hash, display_name, expires_at)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(Uuid::now_v7())
    .bind(token_hash(&pairing_code).to_vec())
    .bind(&display_name)
    .bind(expires_at)
    .execute(state.ticketing.pool())
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "staff pairing code creation failed");
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(PairingCodeResponse {
            version: 2,
            role: "staff",
            display_name,
            pairing_code,
            expires_at: expires_at.unix_timestamp(),
        }),
    )
        .into_response()
}

pub async fn exchange_pairing_code(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ExchangePairingCodeRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(pairing_code) = clean_pairing_code(&payload.pairing_code) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!(%error, "staff pairing transaction failed to start");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let row = sqlx::query_as::<_, PairingCodeRow>(
        r#"
        SELECT id, display_name, expires_at
        FROM staff_pairing_codes
        WHERE workspace_id = $1 AND code_hash = $2 AND used_at IS NULL
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(token_hash(pairing_code).to_vec())
    .fetch_optional(&mut *tx)
    .await;
    let row = match row {
        Ok(Some(row)) if row.expires_at >= OffsetDateTime::now_utc() => row,
        Ok(_) => {
            return Problem::unauthorized(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "staff pairing lookup failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let active_sessions = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM staff_device_sessions
        WHERE workspace_id = $1 AND revoked_at IS NULL AND expires_at > now()
        "#,
    )
    .bind(workspace_id)
    .fetch_one(&mut *tx)
    .await;
    match active_sessions {
        Ok(count) if count < MAX_ACTIVE_DEVICE_SESSIONS => {}
        Ok(_) => {
            return Problem::conflict(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "staff session count failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    }

    let Some(bearer_token) = random_token::<32>() else {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };
    let session_id = Uuid::now_v7();
    let expires_at = OffsetDateTime::now_utc() + Duration::days(DEVICE_SESSION_TTL_DAYS);
    if sqlx::query(
        r#"
        INSERT INTO staff_device_sessions
            (workspace_id, id, token_hash, display_name, expires_at)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(session_id)
    .bind(token_hash(&bearer_token).to_vec())
    .bind(&row.display_name)
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .is_err()
        || sqlx::query(
            "UPDATE staff_pairing_codes SET used_at = now() WHERE workspace_id = $1 AND id = $2 AND used_at IS NULL",
        )
        .bind(workspace_id)
        .bind(row.id)
        .execute(&mut *tx)
        .await
        .is_err()
        || tx.commit().await.is_err()
    {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(DeviceSessionResponse {
            version: 2,
            role: "staff",
            display_name: row.display_name,
            bearer_token,
            session_id,
            expires_at: expires_at.unix_timestamp(),
        }),
    )
        .into_response()
}

pub async fn list_device_sessions(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let rows = sqlx::query_as::<_, DeviceSessionView>(
        r#"
        SELECT id, display_name, expires_at, revoked_at, created_at
        FROM staff_device_sessions
        WHERE workspace_id = $1
        ORDER BY created_at DESC, id DESC
        LIMIT 100
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await;
    match rows {
        Ok(sessions) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(DeviceSessionListResponse { sessions }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "staff session listing failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

pub async fn revoke_device_session(
    State(state): State<crate::AppState>,
    Path(session_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let result = sqlx::query_scalar::<_, i64>(
        r#"
        WITH revoked AS (
            UPDATE staff_device_sessions
            SET revoked_at = COALESCE(revoked_at, now())
            WHERE workspace_id = $1 AND id = $2
            RETURNING token_hash
        ), invalidated_push AS (
            UPDATE fan_push_endpoints endpoint
            SET active = false, invalidated_at = COALESCE(invalidated_at, now()),
                last_error_code = 'staff_session_revoked', updated_at = now()
            FROM revoked
            WHERE endpoint.workspace_id = $1
              AND endpoint.audience_kind = 'staff'
              AND endpoint.principal_hash = revoked.token_hash
              AND endpoint.active
            RETURNING endpoint.id
        )
        SELECT count(*)::bigint FROM revoked
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(session_id)
    .fetch_one(state.ticketing.pool())
    .await;
    match result {
        Ok(1) => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "staff session revocation failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
