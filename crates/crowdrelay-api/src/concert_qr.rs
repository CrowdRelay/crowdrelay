//! Secure concert check-in QR campaigns.
//!
//! Operators create a short-lived, revocable campaign for one published event.
//! The printed URL carries only a domain-separated HMAC token in its fragment;
//! durable PostgreSQL state remains authoritative for time windows, revocation,
//! capacity and one-check-in-per-fan enforcement.

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_application::{
    CheckinCommand, ConcertQrError, ConcertQrRepository, CreateCampaignCommand,
    RevokeCampaignCommand,
};
use crowdrelay_domain::{EventSlug, WorkspaceId};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{Problem, acquisition::fan_session_from_headers, request_id};

type HmacSha256 = Hmac<Sha256>;

const PRIVATE_NO_STORE: &str = "private, no-store";
const TOKEN_VERSION: &str = "v1";
const TOKEN_CONTEXT: &[u8] = b"crowdrelay/concert-checkin/v1";
const MAX_TOKEN_BYTES: usize = 256;
const MAX_CAMPAIGNS_LIMIT: u32 = 100;
const MAX_STAFF_EVENTS_LIMIT: i64 = 100;
const MAX_CAMPAIGN_DURATION: Duration = Duration::days(14);
const EARLIEST_BEFORE_EVENT: Duration = Duration::hours(24);
const LATEST_AFTER_EVENT: Duration = Duration::hours(36);

/// Database and signing material for concert QR routes.
#[derive(Clone)]
pub struct ConcertQrState {
    workspace_id: WorkspaceId,
    database: PgPool,
    signing_key: Option<[u8; 32]>,
}

impl ConcertQrState {
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        database: PgPool,
        root_signing_key: Option<[u8; 32]>,
    ) -> Self {
        // Domain separation prevents a concert token from being accepted by the
        // rotating admission-QR protocol even when both derive from one root.
        let signing_key = root_signing_key.map(|root| {
            let mut digest = Sha256::new();
            digest.update(TOKEN_CONTEXT);
            digest.update(root);
            digest.finalize().into()
        });
        Self {
            workspace_id,
            database,
            signing_key,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCampaignRequest {
    event_slug: String,
    label: String,
    valid_from: String,
    valid_until: String,
    max_checkins: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CampaignListQuery {
    #[serde(default = "default_limit")]
    limit: u32,
}

const fn default_limit() -> u32 {
    50
}

#[derive(Debug, FromRow)]
struct CampaignRow {
    id: Uuid,
    event_id: Uuid,
    event_slug: String,
    event_title: String,
    venue: Option<String>,
    starts_at: OffsetDateTime,
    label: String,
    valid_from: OffsetDateTime,
    valid_until: OffsetDateTime,
    max_checkins: Option<i32>,
    active: bool,
    revoked_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    checkin_count: i64,
}

#[derive(Debug, Serialize)]
pub struct CampaignView {
    id: Uuid,
    event_id: Uuid,
    event_slug: String,
    event_title: String,
    venue: Option<String>,
    starts_at: String,
    label: String,
    valid_from: String,
    valid_until: String,
    max_checkins: Option<u32>,
    checkin_count: u64,
    active: bool,
    revoked_at: Option<String>,
    created_at: String,
    token: Option<String>,
}

#[derive(Debug, Serialize)]
struct CampaignListResponse {
    campaigns: Vec<CampaignView>,
}

#[derive(Debug, FromRow)]
struct StaffEventRow {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    starts_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
struct StaffEventView {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    starts_at: String,
}

#[derive(Debug, Serialize)]
struct ConcertQrOverviewResponse {
    events: Vec<StaffEventView>,
    campaigns: Vec<CampaignView>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckinRequest {
    token: String,
}

#[derive(Debug, Serialize)]
pub struct CheckinResponse {
    event_id: Uuid,
    event_slug: String,
    campaign_id: Uuid,
    created: bool,
    checked_in_at: String,
}

#[derive(Debug, FromRow)]
struct EventRow {
    starts_at: OffsetDateTime,
}

#[derive(Debug)]
struct TokenClaims {
    campaign_id: Uuid,
    event_id: Uuid,
    expires_at: i64,
}

pub async fn create_campaign(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateCampaignRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(signing_key) = state.concert_qr.signing_key else {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Ok(event_slug) = EventSlug::parse(&payload.event_slug) else {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    };
    let label = payload.label.trim();
    if label.is_empty() || label.chars().count() > 160 || label.chars().any(char::is_control) {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }
    let (Ok(valid_from), Ok(valid_until)) = (
        OffsetDateTime::parse(payload.valid_from.trim(), &Rfc3339),
        OffsetDateTime::parse(payload.valid_until.trim(), &Rfc3339),
    ) else {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    };
    let created_at = OffsetDateTime::now_utc();
    if valid_until <= valid_from
        || valid_until <= created_at
        || valid_until - valid_from > MAX_CAMPAIGN_DURATION
        || payload
            .max_checkins
            .is_some_and(|value| !(1..=1_000_000).contains(&value))
    {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }

    // Read-only event lookup for time-window validation against the event
    // start. The repository adapter performs its own locking SELECT within the
    // write transaction; this read only feeds request-level validation.
    let event = match sqlx::query_as::<_, EventRow>(
        r#"
        SELECT starts_at
        FROM events
        WHERE workspace_id = $1 AND slug = $2 AND status = 'published'
        "#,
    )
    .bind(state.concert_qr.workspace_id.into_uuid())
    .bind(event_slug.as_str())
    .fetch_optional(&state.concert_qr.database)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(_) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    if valid_from < event.starts_at - EARLIEST_BEFORE_EVENT
        || valid_until > event.starts_at + LATEST_AFTER_EVENT
    {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }

    let max_checkins = payload
        .max_checkins
        .and_then(|value| i32::try_from(value).ok());
    let command = CreateCampaignCommand {
        workspace_id: state.concert_qr.workspace_id.into_uuid(),
        event_slug: event_slug.as_str().to_owned(),
        label: label.to_owned(),
        valid_from,
        valid_until,
        max_checkins,
        created_at,
        request_id: request_id_value.clone(),
    };
    let result = match state.concert_qr_repo.create_campaign(&command).await {
        Ok(value) => value,
        Err(ConcertQrError::NotFound) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(ConcertQrError::Conflict) => {
            return Problem::conflict(request_id_value)
                .private()
                .into_response();
        }
        Err(ConcertQrError::Invalid) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
        Err(ConcertQrError::Unavailable) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let row = CampaignRow {
        id: result.campaign_id,
        event_id: result.event.id,
        event_slug: result.event.slug,
        event_title: result.event.title,
        venue: result.event.venue,
        starts_at: result.event.starts_at,
        label: label.to_owned(),
        valid_from,
        valid_until,
        max_checkins,
        active: true,
        revoked_at: None,
        created_at: result.created_at,
        checkin_count: 0,
    };
    let view = campaign_view(row, Some(&signing_key));
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(view),
    )
        .into_response()
}

pub async fn list_campaigns(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Result<Query<CampaignListQuery>, QueryRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Query(query) = match query {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    if !(1..=MAX_CAMPAIGNS_LIMIT).contains(&query.limit) {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    }

    let campaigns = match load_campaigns(&state.concert_qr, query.limit).await {
        Ok(rows) => rows
            .into_iter()
            .map(|row| campaign_view(row, state.concert_qr.signing_key.as_ref()))
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "concert QR campaign query failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(CampaignListResponse { campaigns }),
    )
        .into_response()
}

pub async fn overview(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);

    let (event_rows, campaign_rows) = tokio::join!(
        load_staff_events(&state.concert_qr),
        load_campaigns(&state.concert_qr, MAX_CAMPAIGNS_LIMIT),
    );
    let (event_rows, campaign_rows) = match (event_rows, campaign_rows) {
        (Ok(events), Ok(campaigns)) => (events, campaigns),
        (Err(error), _) => {
            tracing::warn!(%error, "concert QR overview event query failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
        (_, Err(error)) => {
            tracing::warn!(%error, "concert QR overview campaign query failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let events = event_rows
        .into_iter()
        .map(|row| StaffEventView {
            id: row.id,
            slug: row.slug,
            title: row.title,
            venue: row.venue,
            starts_at: format_time(row.starts_at),
        })
        .collect();
    let campaigns = campaign_rows
        .into_iter()
        .map(|row| campaign_view(row, state.concert_qr.signing_key.as_ref()))
        .collect();

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ConcertQrOverviewResponse { events, campaigns }),
    )
        .into_response()
}

async fn load_staff_events(state: &ConcertQrState) -> Result<Vec<StaffEventRow>, sqlx::Error> {
    sqlx::query_as::<_, StaffEventRow>(
        r#"
        SELECT id, slug, title, venue, starts_at
        FROM events
        WHERE workspace_id = $1
          AND status = 'published'
          AND starts_at >= now() - interval '36 hours'
        ORDER BY starts_at, id
        LIMIT $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(MAX_STAFF_EVENTS_LIMIT)
    .fetch_all(&state.database)
    .await
}

async fn load_campaigns(
    state: &ConcertQrState,
    limit: u32,
) -> Result<Vec<CampaignRow>, sqlx::Error> {
    sqlx::query_as::<_, CampaignRow>(
        r#"
        SELECT campaign.id, campaign.event_id, event.slug AS event_slug,
               event.title AS event_title, event.venue, event.starts_at,
               campaign.label, campaign.valid_from, campaign.valid_until,
               campaign.max_checkins, campaign.active, campaign.revoked_at,
               campaign.created_at, count(checkin.id)::bigint AS checkin_count
        FROM concert_qr_campaigns AS campaign
        INNER JOIN events AS event
          ON event.workspace_id = campaign.workspace_id AND event.id = campaign.event_id
        LEFT JOIN concert_checkins AS checkin
          ON checkin.workspace_id = campaign.workspace_id AND checkin.campaign_id = campaign.id
        WHERE campaign.workspace_id = $1
        GROUP BY campaign.id, event.id
        ORDER BY campaign.created_at DESC, campaign.id DESC
        LIMIT $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(i64::from(limit))
    .fetch_all(&state.database)
    .await
}

pub async fn revoke_campaign(
    State(state): State<crate::AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let Ok(campaign_id) = Uuid::parse_str(&raw_id) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let command = RevokeCampaignCommand {
        workspace_id: state.concert_qr.workspace_id.into_uuid(),
        campaign_id,
        request_id: request_id_value.clone(),
    };
    match state.concert_qr_repo.revoke_campaign(&command).await {
        Ok(()) => (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response(),
        Err(ConcertQrError::NotFound) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(ConcertQrError::Conflict) => Problem::conflict(request_id_value)
            .private()
            .into_response(),
        Err(ConcertQrError::Invalid) => Problem::unprocessable(request_id_value)
            .private()
            .into_response(),
        Err(ConcertQrError::Unavailable) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn check_in(
    State(state): State<crate::AppState>,
    Path(raw_slug): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<CheckinRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Some(session) = fan_session_from_headers(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let Some(signing_key) = state.concert_qr.signing_key else {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };
    let Ok(event_slug) = EventSlug::parse(raw_slug) else {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(claims) = verify_token(payload.token.trim(), &signing_key) else {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    };
    let now = OffsetDateTime::now_utc();
    if claims.expires_at < now.unix_timestamp() {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    }

    let command = CheckinCommand {
        workspace_id: state.concert_qr.workspace_id.into_uuid(),
        event_slug: event_slug.as_str().to_owned(),
        campaign_id: claims.campaign_id,
        event_id: claims.event_id,
        expires_at: claims.expires_at,
        session_token: session.as_str().to_owned(),
        now,
        request_id: request_id_value.clone(),
    };
    let result = match state.concert_qr_repo.check_in(&command).await {
        Ok(value) => value,
        Err(ConcertQrError::NotFound) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(ConcertQrError::Conflict) => {
            return Problem::conflict(request_id_value)
                .private()
                .into_response();
        }
        Err(ConcertQrError::Invalid) => {
            return Problem::unprocessable(request_id_value)
                .private()
                .into_response();
        }
        Err(ConcertQrError::Unavailable) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let status = if result.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (
        status,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(CheckinResponse {
            event_id: result.event_id,
            event_slug: result.event_slug,
            campaign_id: result.campaign_id,
            created: result.created,
            checked_in_at: format_time(result.checked_in_at),
        }),
    )
        .into_response()
}

fn campaign_view(row: CampaignRow, signing_key: Option<&[u8; 32]>) -> CampaignView {
    let effective_active =
        row.active && row.revoked_at.is_none() && row.valid_until > OffsetDateTime::now_utc();
    let token = if effective_active {
        signing_key.and_then(|key| sign_token(row.id, row.event_id, row.valid_until, key))
    } else {
        None
    };
    CampaignView {
        id: row.id,
        event_id: row.event_id,
        event_slug: row.event_slug,
        event_title: row.event_title,
        venue: row.venue,
        starts_at: format_time(row.starts_at),
        label: row.label,
        valid_from: format_time(row.valid_from),
        valid_until: format_time(row.valid_until),
        max_checkins: row.max_checkins.and_then(|value| u32::try_from(value).ok()),
        checkin_count: u64::try_from(row.checkin_count).unwrap_or_default(),
        active: effective_active,
        revoked_at: row.revoked_at.map(format_time),
        created_at: format_time(row.created_at),
        token,
    }
}

fn format_time(value: OffsetDateTime) -> String {
    value
        .format(&Rfc3339)
        .unwrap_or_else(|_| value.unix_timestamp().to_string())
}

fn sign_token(
    campaign_id: Uuid,
    event_id: Uuid,
    valid_until: OffsetDateTime,
    signing_key: &[u8; 32],
) -> Option<String> {
    let unsigned = format!(
        "{TOKEN_VERSION}.{}.{}.{}",
        campaign_id,
        event_id,
        valid_until.unix_timestamp()
    );
    let mut mac = HmacSha256::new_from_slice(signing_key).ok()?;
    mac.update(unsigned.as_bytes());
    Some(format!(
        "{unsigned}.{}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

fn verify_token(token: &str, signing_key: &[u8; 32]) -> Option<TokenClaims> {
    if token.len() > MAX_TOKEN_BYTES || !token.is_ascii() {
        return None;
    }
    let mut parts = token.split('.');
    let version = parts.next()?;
    let campaign_id = Uuid::parse_str(parts.next()?).ok()?;
    let event_id = Uuid::parse_str(parts.next()?).ok()?;
    let expires_at = parts.next()?.parse::<i64>().ok()?;
    let signature = hex::decode(parts.next()?).ok()?;
    if version != TOKEN_VERSION || parts.next().is_some() || signature.len() != 32 {
        return None;
    }
    let unsigned = format!("{version}.{campaign_id}.{event_id}.{expires_at}");
    let mut mac = HmacSha256::new_from_slice(signing_key).ok()?;
    mac.update(unsigned.as_bytes());
    mac.verify_slice(&signature).ok()?;
    Some(TokenClaims {
        campaign_id,
        event_id,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips_and_rejects_tampering() {
        let key = [7_u8; 32];
        let campaign_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let expiry = OffsetDateTime::UNIX_EPOCH + Duration::hours(1);
        let token = sign_token(campaign_id, event_id, expiry, &key).expect("token");
        let claims = verify_token(&token, &key).expect("valid token");
        assert_eq!(claims.campaign_id, campaign_id);
        assert_eq!(claims.event_id, event_id);
        assert_eq!(claims.expires_at, expiry.unix_timestamp());
        assert!(verify_token(&format!("{token}x"), &key).is_none());
        assert!(verify_token(&token, &[8_u8; 32]).is_none());
    }
}
