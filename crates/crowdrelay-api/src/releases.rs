//! Trusted release announcements created by a provider watcher.
//!
//! n8n detects a new artist release, but it never receives or enumerates the fan
//! list. This endpoint validates and deduplicates the release, resolves the
//! current Signal audience inside the CrowdRelay transaction, and appends one
//! durable outbox event per currently eligible address.

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Postgres, Transaction};
use time::{Date, Month};
use tokio::time::timeout;
use url::Url;
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_RECIPIENTS: i64 = 100_000;
const IDEMPOTENCY_SCOPE: &str = "internal.release.announce";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnounceReleaseRequest {
    source: String,
    source_release_id: String,
    title: String,
    release_type: String,
    release_date: String,
    listen_url: String,
    image_url: Option<String>,
    site_url: Option<String>,
    artist_name: Option<String>,
    total_tracks: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AnnounceReleaseResponse {
    accepted: bool,
    duplicate: bool,
    source_release_id: String,
    recipient_count: i64,
}

#[derive(Clone, Debug, Serialize)]
struct NormalizedRelease {
    id: String,
    source: String,
    title: String,
    release_type: String,
    release_date: String,
    listen_url: String,
    image_url: Option<String>,
    site_url: Option<String>,
    artist_name: String,
    total_tracks: Option<i32>,
}

#[derive(Debug, FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    state: String,
    response_body: Option<Value>,
}

pub async fn announce_release(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<AnnounceReleaseRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let release = match normalize_request(payload) {
        Some(value) => value,
        None => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };

    let future = announce_release_inner(&state, release);
    match timeout(
        state.ticketing.operation_timeout().saturating_mul(3),
        future,
    )
    .await
    {
        Ok(Ok(response)) => (
            StatusCode::ACCEPTED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(response),
        )
            .into_response(),
        Ok(Err(AnnounceReleaseError::Conflict)) => Problem::conflict(request_id_value)
            .private()
            .into_response(),
        Ok(Err(AnnounceReleaseError::Unavailable | AnnounceReleaseError::InProgress)) | Err(_) => {
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

fn normalize_request(request: AnnounceReleaseRequest) -> Option<NormalizedRelease> {
    let source = clean_text(&request.source, 32)?.to_ascii_lowercase();
    if source != "spotify" {
        return None;
    }
    let source_release_id = clean_identifier(&request.source_release_id, 200)?;
    let title = clean_text(&request.title, 200)?;
    let release_type = clean_text(&request.release_type, 16)?.to_ascii_lowercase();
    if !matches!(release_type.as_str(), "single" | "ep" | "album") {
        return None;
    }
    let release_date = request.release_date.trim().to_owned();
    if !valid_date(&release_date) {
        return None;
    }
    let listen_url = clean_http_url(&request.listen_url)?;
    let image_url = match request.image_url.as_deref() {
        Some(value) => Some(clean_http_url(value)?),
        None => None,
    };
    let site_url = match request.site_url.as_deref() {
        Some(value) => Some(clean_http_url(value)?),
        None => None,
    };
    let artist_name = match request.artist_name.as_deref() {
        Some(value) => clean_text(value, 120)?,
        None => "Artist".to_owned(),
    };
    if request
        .total_tracks
        .is_some_and(|value| !(1..=500).contains(&value))
    {
        return None;
    }
    Some(NormalizedRelease {
        id: source_release_id,
        source,
        title,
        release_type,
        release_date,
        listen_url,
        image_url,
        site_url,
        artist_name,
        total_tracks: request.total_tracks,
    })
}

fn clean_text(value: &str, maximum_chars: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > maximum_chars
        || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(value.to_owned())
}

fn clean_identifier(value: &str, maximum_chars: usize) -> Option<String> {
    let value = clean_text(value, maximum_chars)?;
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        .then_some(value)
}

fn clean_http_url(value: &str) -> Option<String> {
    let value = value.trim();
    let url = Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| value.to_owned())
}

fn valid_date(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let (Ok(year), Ok(month), Ok(day)) =
        (year.parse::<i32>(), month.parse::<u8>(), day.parse::<u8>())
    else {
        return false;
    };
    let Ok(month) = Month::try_from(month) else {
        return false;
    };
    Date::from_calendar_date(year, month, day).is_ok()
}

async fn announce_release_inner(
    state: &crate::AppState,
    release: NormalizedRelease,
) -> Result<AnnounceReleaseResponse, AnnounceReleaseError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let release_payload =
        serde_json::to_value(&release).map_err(|_| AnnounceReleaseError::Unavailable)?;
    let canonical =
        serde_json::to_vec(&release_payload).map_err(|_| AnnounceReleaseError::Unavailable)?;
    let request_hash: [u8; 32] = Sha256::digest(canonical).into();
    let key = format!("{}:{}", release.source, release.id);
    let lease_owner = Uuid::now_v7().to_string();

    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(|_| AnnounceReleaseError::Unavailable)?;
    configure_transaction(&mut transaction, state).await?;

    let claimed = sqlx::query_scalar::<_, i32>(
        r#"
        INSERT INTO idempotency_keys (
            workspace_id, scope, key, request_hash, state,
            lease_owner, lease_expires_at, expires_at
        )
        VALUES (
            $1, $2, $3, $4, 'in_progress',
            $5, now() + interval '5 minutes', now() + interval '10 years'
        )
        ON CONFLICT (workspace_id, scope, key) DO NOTHING
        RETURNING 1
        "#,
    )
    .bind(workspace_id)
    .bind(IDEMPOTENCY_SCOPE)
    .bind(&key)
    .bind(request_hash.as_slice())
    .bind(&lease_owner)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| AnnounceReleaseError::Unavailable)?;

    if claimed.is_none() {
        let existing = sqlx::query_as::<_, IdempotencyRow>(
            r#"
            SELECT request_hash, state, response_body
            FROM idempotency_keys
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(IDEMPOTENCY_SCOPE)
        .bind(&key)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| AnnounceReleaseError::Unavailable)?;
        if existing.request_hash.as_slice() != request_hash.as_slice() {
            return Err(AnnounceReleaseError::Conflict);
        }
        if existing.state != "completed" {
            return Err(AnnounceReleaseError::InProgress);
        }
        let mut response: AnnounceReleaseResponse = serde_json::from_value(
            existing
                .response_body
                .ok_or(AnnounceReleaseError::Unavailable)?,
        )
        .map_err(|_| AnnounceReleaseError::Unavailable)?;
        response.duplicate = true;
        transaction
            .commit()
            .await
            .map_err(|_| AnnounceReleaseError::Unavailable)?;
        return Ok(response);
    }

    let recipient_count = sqlx::query_scalar::<_, i64>(
        r#"
        WITH latest_marketing_consent AS (
            SELECT DISTINCT ON (consent.fan_id)
                consent.fan_id,
                consent.granted
            FROM fan_consents AS consent
            WHERE consent.workspace_id = $1
              AND consent.purpose = 'marketing'
            ORDER BY consent.fan_id, consent.recorded_at DESC, consent.id DESC
        ), candidates AS (
            SELECT DISTINCT ON (fan.normalized_email)
                fan.id,
                fan.normalized_email,
                fan.display_name,
                fan.locale
            FROM fans AS fan
            JOIN latest_marketing_consent AS consent
              ON consent.fan_id = fan.id
             AND consent.granted
            WHERE fan.workspace_id = $1
              AND fan.status = 'active'
              AND fan.normalized_email <> ''
            ORDER BY fan.normalized_email, fan.id
            LIMIT $4
        ), inserted AS (
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version,
                payload, request_id, available_at
            )
            SELECT
                $1,
                'release.announcement_due',
                1,
                jsonb_build_object(
                    'fan', jsonb_build_object(
                        'id', recipient.id,
                        'email', recipient.normalized_email,
                        'display_name', recipient.display_name,
                        'locale', recipient.locale
                    ),
                    'release', $3::jsonb
                ),
                'release:' || $2 || ':fan:' || recipient.id::text,
                now() + interval '30 seconds'
            FROM candidates AS recipient
            RETURNING 1
        )
        SELECT count(*)::bigint FROM inserted
        "#,
    )
    .bind(workspace_id)
    .bind(&key)
    .bind(&release_payload)
    .bind(MAX_RECIPIENTS)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| AnnounceReleaseError::Unavailable)?;

    // Feed the trusted release fact into ViryaOS Content Supply inside the
    // same transaction. This is a projection only; Rust policy still decides
    // which artifacts are due and n8n only executes the resulting request.
    sqlx::query(
        r#"
        INSERT INTO viryaos_content_sources(
            workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
        ) VALUES(
            $1,'release',$2,$3,
            (($4::date)::timestamp AT TIME ZONE 'UTC'),
            (($4::date)::timestamp AT TIME ZONE 'UTC') + INTERVAL '45 days',
            $5,true
        )
        ON CONFLICT(workspace_id,source_kind,source_key) DO UPDATE SET
            title=EXCLUDED.title,
            occurred_at=EXCLUDED.occurred_at,
            expires_at=EXCLUDED.expires_at,
            metadata=EXCLUDED.metadata,
            active=true,
            version=viryaos_content_sources.version+1
        "#,
    )
    .bind(workspace_id)
    .bind(format!("{}:{}", release.source, release.id))
    .bind(&release.title)
    .bind(&release.release_date)
    .bind(&release_payload)
    .execute(&mut *transaction)
    .await
    .map_err(|_| AnnounceReleaseError::Unavailable)?;

    let response = AnnounceReleaseResponse {
        accepted: true,
        duplicate: false,
        source_release_id: release.id.clone(),
        recipient_count,
    };
    let response_body =
        serde_json::to_value(&response).map_err(|_| AnnounceReleaseError::Unavailable)?;

    sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET state = 'completed',
            lease_owner = NULL,
            lease_expires_at = NULL,
            response_status = 202,
            response_body = $5,
            response_content_type = 'application/json',
            completed_at = now()
        WHERE workspace_id = $1
          AND scope = $2
          AND key = $3
          AND lease_owner = $4
          AND state = 'in_progress'
        "#,
    )
    .bind(workspace_id)
    .bind(IDEMPOTENCY_SCOPE)
    .bind(&key)
    .bind(&lease_owner)
    .bind(&response_body)
    .execute(&mut *transaction)
    .await
    .map_err(|_| AnnounceReleaseError::Unavailable)?;

    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, metadata
        )
        VALUES ($1, 'service', 'release.announcement_enqueued', 'release', $2, $3)
        "#,
    )
    .bind(workspace_id)
    .bind(&key)
    .bind(json!({
        "source": release.source,
        "title": release.title,
        "release_date": release.release_date,
        "release_type": release.release_type,
        "recipient_count": recipient_count,
    }))
    .execute(&mut *transaction)
    .await
    .map_err(|_| AnnounceReleaseError::Unavailable)?;

    transaction
        .commit()
        .await
        .map_err(|_| AnnounceReleaseError::Unavailable)?;
    Ok(response)
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
) -> Result<(), AnnounceReleaseError> {
    let statement_ms = state
        .ticketing
        .operation_timeout()
        .saturating_mul(3)
        .as_millis();
    let lock_ms = state.ticketing.lock_timeout().as_millis();
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(|_| AnnounceReleaseError::Unavailable)?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum AnnounceReleaseError {
    Conflict,
    InProgress,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_request_is_bounded_and_validated() {
        let normalized = normalize_request(AnnounceReleaseRequest {
            source: "spotify".to_owned(),
            source_release_id: "4AbCdEf123".to_owned(),
            title: "Example Release".to_owned(),
            release_type: "single".to_owned(),
            release_date: "2030-12-04".to_owned(),
            listen_url: "https://open.spotify.com/album/4AbCdEf123".to_owned(),
            image_url: Some("https://i.scdn.co/image/example".to_owned()),
            site_url: Some("https://artist.example/".to_owned()),
            artist_name: Some("Example Artist".to_owned()),
            total_tracks: Some(1),
        })
        .expect("valid release");
        assert_eq!(normalized.release_type, "single");
        assert!(
            normalize_request(AnnounceReleaseRequest {
                source: "spotify".to_owned(),
                source_release_id: "bad/id".to_owned(),
                title: "Release".to_owned(),
                release_type: "single".to_owned(),
                release_date: "2026-02-30".to_owned(),
                listen_url: "javascript:alert(1)".to_owned(),
                image_url: None,
                site_url: None,
                artist_name: None,
                total_tracks: Some(1),
            })
            .is_none()
        );
    }
}
