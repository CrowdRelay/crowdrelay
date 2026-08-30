//! Signal "Latarnik" access for existing Virya OS Beacons.
//!
//! The relationship record remains `viryaos_beacons`; this module only owns
//! invitation/session auth, self-service preferences, the press-room read model
//! and a bounded nearby-show push wave. This keeps Beacon outreach cadence in
//! Autopilot and avoids creating a second CRM or treating media contacts as fans.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crowdrelay_application::BeaconSignalRepository;

use crate::{Problem, request_id};

mod helpers;
mod invite_copy;
mod lifecycle;
mod network;
mod releases;
use helpers::{
    clean_locale, clean_topics, random_token, token_hash, valid_invite_token, valid_radius,
};
use invite_copy::{InviteDeliveryCopy, invite_delivery_copy};
pub use lifecycle::{
    admin_candidates, admin_coverage, admin_dashboard, admin_engagements, admin_press_assets,
    admin_press_requests, admin_resolve_press_request, admin_set_state, admin_upsert_press_asset,
    create_invite_batch, leave, my_press_requests, press_room, record_event_engagement,
    submit_coverage,
};
pub use network::{
    admin_beacon_network, admin_beacon_network_action, internal_claim_invite_delivery_job,
    internal_ingest_discovered_beacons, internal_report_discovery_run,
    internal_report_invite_delivery_job,
};
pub use releases::{
    admin_close_release_campaign, admin_create_release_campaign, admin_launch_release_campaign,
    admin_list_release_campaigns, admin_list_release_recipients, admin_update_release_recipient,
    confirm_release_delivery, decline_release_delivery, my_release_campaigns,
};

const PRIVATE_NO_STORE: &str = "private, no-store";
const DEFAULT_INVITE_TTL_DAYS: i64 = 14;
const MAX_INVITE_TTL_DAYS: i64 = 30;
const SESSION_TTL_DAYS: i64 = 180;
const DEFAULT_RADIUS_KM: i32 = 100;
const DEFAULT_WAVE_SIZE: i64 = 20;
const MAX_WAVE_SIZE: i64 = 100;

#[derive(Debug)]
pub(crate) enum BeaconSignalError {
    Unauthorized,
    BadRequest,
    Conflict,
    NotFound,
    Unavailable,
}

impl BeaconSignalError {
    fn response(self, request_id_value: Option<String>) -> Response {
        match self {
            Self::Unauthorized => Problem::unauthorized(request_id_value)
                .private()
                .into_response(),
            Self::BadRequest => Problem::bad_request(request_id_value)
                .private()
                .into_response(),
            Self::Conflict => Problem::conflict(request_id_value)
                .private()
                .into_response(),
            Self::NotFound => Problem::not_found(request_id_value)
                .private()
                .into_response(),
            Self::Unavailable => Problem::service_unavailable(request_id_value)
                .private()
                .into_response(),
        }
    }
}

fn map_repo_error(error: crowdrelay_application::BeaconSignalRepositoryError) -> BeaconSignalError {
    match error {
        crowdrelay_application::BeaconSignalRepositoryError::NotFound => {
            BeaconSignalError::NotFound
        }
        crowdrelay_application::BeaconSignalRepositoryError::Conflict => {
            BeaconSignalError::Conflict
        }
        crowdrelay_application::BeaconSignalRepositoryError::BadRequest => {
            BeaconSignalError::BadRequest
        }
        crowdrelay_application::BeaconSignalRepositoryError::Unavailable => {
            BeaconSignalError::Unavailable
        }
    }
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct BeaconPrincipal {
    pub beacon_id: Uuid,
    pub session_hash: Vec<u8>,
    pub display_name: String,
    pub beacon_kind: String,
    pub city_id: Option<Uuid>,
    pub radius_km: i32,
    pub locale: String,
    pub topics: Vec<String>,
    pub nearby_gigs_enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CreateInviteRequest {
    #[serde(default = "default_invite_ttl_days")]
    ttl_days: i64,
    #[serde(default = "default_radius_km")]
    radius_km: i32,
    #[serde(default = "default_locale")]
    locale: String,
}

fn default_invite_ttl_days() -> i64 {
    DEFAULT_INVITE_TTL_DAYS
}

fn default_radius_km() -> i32 {
    DEFAULT_RADIUS_KM
}

fn default_locale() -> String {
    "pl".to_owned()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateInviteResponse {
    version: u8,
    beacon_id: Uuid,
    invite_token: String,
    invite_url: String,
    display_name: String,
    delivery: InviteDeliveryCopy,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExchangeInviteRequest {
    invite_token: String,
    #[serde(default)]
    radius_km: Option<i32>,
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    topics: Option<Vec<String>>,
    #[serde(default)]
    client_kind: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeInviteResponse {
    version: u8,
    role: &'static str,
    beacon_id: Uuid,
    display_name: String,
    beacon_kind: String,
    bearer_token: String,
    session_id: Uuid,
    client_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    preferences: BeaconPreferences,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BeaconPreferences {
    radius_km: i32,
    locale: String,
    topics: Vec<String>,
    nearby_gigs_enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpdatePreferencesRequest {
    radius_km: Option<i32>,
    locale: Option<String>,
    topics: Option<Vec<String>>,
    nearby_gigs_enabled: Option<bool>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct NearbyEvent {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    city: String,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    ticket_url: Option<String>,
    distance_km: i32,
    engagement_status: Option<String>,
    help_kind: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_notified_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PressRoom {
    home_url: String,
    epk_url: String,
    gallery_url: String,
    rider_url: String,
    spotify_url: String,
    youtube_url: String,
    contact_email: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BeaconMeResponse {
    version: u8,
    role: &'static str,
    beacon_id: Uuid,
    display_name: String,
    beacon_kind: String,
    city: Option<String>,
    preferences: BeaconPreferences,
    nearby_events: Vec<NearbyEvent>,
    press_room: PressRoom,
    open_press_requests: i64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PressRequestKind {
    PressPhoto,
    Wav,
    CleanVersion,
    Interview,
    Accreditation,
    Custom,
}

impl PressRequestKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PressPhoto => "press_photo",
            Self::Wav => "wav",
            Self::CleanVersion => "clean_version",
            Self::Interview => "interview",
            Self::Accreditation => "accreditation",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CreatePressRequest {
    event_id: Option<Uuid>,
    request_kind: PressRequestKind,
    details: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PressRequestResponse {
    request_id: Uuid,
    status: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EmitNearbyRequest {
    #[serde(default = "default_wave_size")]
    limit: i64,
    #[serde(default = "default_lead_days")]
    lead_days: i64,
}

fn default_wave_size() -> i64 {
    DEFAULT_WAVE_SIZE
}

fn default_lead_days() -> i64 {
    60
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EmitNearbyResponse {
    eligible: i64,
    push_queued: i64,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
struct AdminPressRequestView {
    id: Uuid,
    beacon_id: Uuid,
    display_name: String,
    beacon_kind: String,
    event_id: Option<Uuid>,
    event_title: Option<String>,
    request_kind: String,
    details: Option<String>,
    status: String,
    resolution_note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    resolved_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminPressRequestsResponse {
    requests: Vec<AdminPressRequestView>,
}

pub(crate) async fn authorize_beacon(
    state: &crate::AppState,
    headers: &HeaderMap,
) -> Result<BeaconPrincipal, BeaconSignalError> {
    let Some(session_hash) = crate::security::bearer_sha256(headers) else {
        return Err(BeaconSignalError::Unauthorized);
    };
    let row = sqlx::query_as::<_, BeaconPrincipal>(
        r#"
        SELECT session.beacon_id,
               session.token_hash AS session_hash,
               beacon.display_name,
               beacon.beacon_kind,
               beacon.city_id,
               profile.radius_km,
               profile.locale,
               profile.topics,
               profile.nearby_gigs_enabled
        FROM viryaos_beacon_signal_sessions session
        JOIN viryaos_beacon_signal_profiles profile
          ON profile.workspace_id = session.workspace_id
         AND profile.beacon_id = session.beacon_id
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id = session.workspace_id
         AND beacon.id = session.beacon_id
        WHERE session.workspace_id = $1
          AND session.token_hash = $2
          AND session.revoked_at IS NULL
          AND session.expires_at > now()
          AND profile.status = 'active'
          AND beacon.active
          AND beacon.verified
          AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(session_hash.to_vec())
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(|error| {
        tracing::warn!(%error, "Beacon Signal session lookup failed");
        BeaconSignalError::Unavailable
    })?;
    row.ok_or(BeaconSignalError::Unauthorized)
}

pub async fn create_invite(
    State(state): State<crate::AppState>,
    Path(beacon_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<CreateInviteRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let Some(locale) = clean_locale(&payload.locale) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    if !(1..=MAX_INVITE_TTL_DAYS).contains(&payload.ttl_days) || !valid_radius(payload.radius_km) {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }

    let Some(invite_token) = random_token::<24>() else {
        return BeaconSignalError::Unavailable.response(request_id_value);
    };
    let expires_at = OffsetDateTime::now_utc() + Duration::days(payload.ttl_days);
    let command = crowdrelay_application::CreateInviteCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        beacon_id,
        invite_token_hash: token_hash(&invite_token),
        invite_expires_at: expires_at,
        radius_km: payload.radius_km,
        locale: locale.clone(),
    };
    let result = state.beacon_release.create_invite(&command).await;
    let display_name = match result {
        Ok(result) => result.display_name,
        Err(error) => return map_repo_error(error).response(request_id_value),
    };

    let path = if locale.starts_with("pl") {
        "pl/latarnik"
    } else {
        "latarnik"
    };
    let invite_url = format!("https://virya.music/{path}?invite={invite_token}");
    let delivery = invite_delivery_copy(&locale, &display_name, &invite_url);
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(CreateInviteResponse {
            version: 2,
            beacon_id,
            display_name,
            invite_token,
            invite_url,
            delivery,
            expires_at,
        }),
    )
        .into_response()
}

pub async fn exchange_invite(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ExchangeInviteRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if !valid_invite_token(&payload.invite_token) {
        return BeaconSignalError::Unauthorized.response(request_id_value);
    }
    let locale = match payload.locale.as_deref() {
        Some(value) => match clean_locale(value) {
            Some(value) => Some(value),
            None => return BeaconSignalError::BadRequest.response(request_id_value),
        },
        None => None,
    };
    if payload.radius_km.is_some_and(|value| !valid_radius(value)) {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let topics = match payload.topics {
        Some(values) => match clean_topics(values) {
            Some(values) => Some(values),
            None => return BeaconSignalError::BadRequest.response(request_id_value),
        },
        None => None,
    };
    let client_kind = payload.client_kind.as_deref().unwrap_or("web").trim();
    if !matches!(client_kind, "web" | "android" | "ios") {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let Some(bearer_token) = random_token::<32>() else {
        return BeaconSignalError::Unavailable.response(request_id_value);
    };
    let session_id = Uuid::now_v7();
    let expires_at = OffsetDateTime::now_utc() + Duration::days(SESSION_TTL_DAYS);
    let command = crowdrelay_application::ExchangeInviteCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        invite_token_hash: token_hash(payload.invite_token.trim()),
        bearer_token_hash: token_hash(&bearer_token),
        session_id,
        session_expires_at: expires_at,
        client_kind: client_kind.to_owned(),
        locale,
        radius_km: payload.radius_km,
        topics,
    };
    let result = state.beacon_release.exchange_invite(&command).await;
    let exchange = match result {
        Ok(value) => value,
        Err(error) => return map_repo_error(error).response(request_id_value),
    };
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ExchangeInviteResponse {
            version: 1,
            role: "beacon",
            beacon_id: exchange.beacon_id,
            display_name: exchange.display_name,
            beacon_kind: exchange.beacon_kind,
            bearer_token,
            session_id,
            client_kind: command.client_kind,
            expires_at,
            preferences: BeaconPreferences {
                radius_km: exchange.radius_km,
                locale: exchange.locale,
                topics: exchange.topics,
                nearby_gigs_enabled: exchange.nearby_gigs_enabled,
            },
        }),
    )
        .into_response()
}

pub async fn me(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let city = match principal.city_id {
        Some(city_id) => sqlx::query_scalar::<_, String>("SELECT name FROM cities WHERE id=$1")
            .bind(city_id)
            .fetch_optional(state.ticketing.pool())
            .await
            .unwrap_or(None),
        None => None,
    };
    let nearby_events = if principal.city_id.is_none()
        || !principal.nearby_gigs_enabled
        || !principal.topics.iter().any(|topic| topic == "shows")
    {
        Ok(Vec::new())
    } else {
        sqlx::query_as::<_, NearbyEvent>(
            r#"
            SELECT event.id, event.slug, event.title, event.venue, event_city.name AS city,
                   event.starts_at, event.ticket_url,
                   LEAST(20000, ROUND(
                       6371 * 2 * ASIN(LEAST(1.0, SQRT(
                           POWER(SIN(RADIANS(home_city.latitude - event_city.latitude) / 2), 2)
                           + COS(RADIANS(event_city.latitude)) * COS(RADIANS(home_city.latitude))
                           * POWER(SIN(RADIANS(home_city.longitude - event_city.longitude) / 2), 2)
                       )))
                   )::integer) AS distance_km,
                   engagement.status AS engagement_status, engagement.help_kind,
                   engagement.last_notified_at
            FROM viryaos_beacons beacon
            JOIN cities home_city ON home_city.id = beacon.city_id
            JOIN events event ON event.workspace_id = beacon.workspace_id
                AND event.status='published' AND event.starts_at > now()
                AND event.starts_at < now() + interval '365 days'
            JOIN cities event_city ON event_city.id = event.city_id
            LEFT JOIN viryaos_beacon_signal_event_engagements engagement
              ON engagement.workspace_id=event.workspace_id
             AND engagement.beacon_id=beacon.id AND engagement.event_id=event.id
            WHERE beacon.workspace_id=$1 AND beacon.id=$2
              AND home_city.latitude IS NOT NULL AND home_city.longitude IS NOT NULL
              AND event_city.latitude IS NOT NULL AND event_city.longitude IS NOT NULL
              AND (6371 * 2 * ASIN(LEAST(1.0, SQRT(
                    POWER(SIN(RADIANS(home_city.latitude - event_city.latitude) / 2), 2)
                    + COS(RADIANS(event_city.latitude)) * COS(RADIANS(home_city.latitude))
                    * POWER(SIN(RADIANS(home_city.longitude - event_city.longitude) / 2), 2)
                  )))) <= $3
            ORDER BY event.starts_at, event.id
            LIMIT 20
            "#,
        )
        .bind(workspace_id)
        .bind(principal.beacon_id)
        .bind(principal.radius_km)
        .fetch_all(state.ticketing.pool())
        .await
    };
    let nearby_events = match nearby_events {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon nearby-event read failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let open_press_requests = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM viryaos_beacon_press_requests WHERE workspace_id=$1 AND beacon_id=$2 AND status='open'",
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .fetch_one(state.ticketing.pool())
    .await
    .unwrap_or(0);
    let pl = principal.locale.starts_with("pl");
    let root = if pl {
        "https://virya.music/pl"
    } else {
        "https://virya.music"
    };
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(BeaconMeResponse {
            version: 1,
            role: "beacon",
            beacon_id: principal.beacon_id,
            display_name: principal.display_name,
            beacon_kind: principal.beacon_kind,
            city,
            preferences: BeaconPreferences {
                radius_km: principal.radius_km,
                locale: principal.locale,
                topics: principal.topics,
                nearby_gigs_enabled: principal.nearby_gigs_enabled,
            },
            nearby_events,
            press_room: PressRoom {
                home_url: format!("{root}/latarnik"),
                epk_url: format!("{root}/epk"),
                gallery_url: format!("{root}/gallery"),
                rider_url: "https://virya.music/techrider.pdf".to_owned(),
                spotify_url: "https://open.spotify.com/artist/6bbW0jOKAWJWm3h6CTWaAS".to_owned(),
                youtube_url: "https://www.youtube.com/@ViryaOfficial".to_owned(),
                contact_email: "virya.crew@gmail.com",
            },
            open_press_requests,
        }),
    )
        .into_response()
}

pub async fn update_preferences(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<UpdatePreferencesRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if payload.radius_km.is_none()
        && payload.locale.is_none()
        && payload.topics.is_none()
        && payload.nearby_gigs_enabled.is_none()
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    if payload.radius_km.is_some_and(|value| !valid_radius(value)) {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let locale = match payload.locale.as_deref() {
        Some(value) => match clean_locale(value) {
            Some(value) => Some(value),
            None => return BeaconSignalError::BadRequest.response(request_id_value),
        },
        None => None,
    };
    let topics = match payload.topics {
        Some(values) => match clean_topics(values) {
            Some(values) => Some(values),
            None => return BeaconSignalError::BadRequest.response(request_id_value),
        },
        None => None,
    };
    let command = crowdrelay_application::UpdatePreferencesCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        beacon_id: principal.beacon_id,
        radius_km: payload.radius_km,
        locale,
        topics,
        nearby_gigs_enabled: payload.nearby_gigs_enabled,
    };
    let result = state.beacon_release.update_preferences(&command).await;
    match result {
        Ok(Some(preferences)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(BeaconPreferences {
                radius_km: preferences.radius_km,
                locale: preferences.locale,
                topics: preferences.topics,
                nearby_gigs_enabled: preferences.nearby_gigs_enabled,
            }),
        )
            .into_response(),
        Ok(None) => BeaconSignalError::Unauthorized.response(request_id_value),
        Err(error) => map_repo_error(error).response(request_id_value),
    }
}

pub async fn create_press_request(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreatePressRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let details = payload.details.map(|value| value.trim().to_owned());
    if details
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.chars().count() > 1500)
        || (matches!(payload.request_kind, PressRequestKind::Custom) && details.is_none())
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let command = crowdrelay_application::CreatePressRequestCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        beacon_id: principal.beacon_id,
        event_id: payload.event_id,
        request_kind: payload.request_kind.as_str().to_owned(),
        details,
        request_id_header: request_id_value.clone(),
    };
    let result = state.beacon_release.create_press_request(&command).await;
    match result {
        Ok(result) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PressRequestResponse {
                request_id: result.request_id,
                status: "open",
            }),
        )
            .into_response(),
        Err(error) => map_repo_error(error).response(request_id_value),
    }
}

pub async fn logout(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let command = crowdrelay_application::LogoutCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        session_hash: principal.session_hash,
    };
    match state.beacon_release.logout(&command).await {
        Ok(()) => (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response(),
        Err(error) => map_repo_error(error).response(request_id_value),
    }
}

pub async fn emit_nearby(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<EmitNearbyRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if !(1..=MAX_WAVE_SIZE).contains(&payload.limit) || !(1..=180).contains(&payload.lead_days) {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let push_enabled = if state.push.runtime_enabled {
        crate::ecosystem::feature_enabled(&state, "push_delivery_enabled")
            .await
            .unwrap_or(false)
    } else {
        false
    };
    let command = crowdrelay_application::EmitNearbyCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        limit: payload.limit,
        lead_days: payload.lead_days,
        push_enabled,
    };
    let result = state.beacon_release.emit_nearby(&command).await;
    match result {
        Ok(result) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(EmitNearbyResponse {
                eligible: result.eligible,
                push_queued: result.push_queued,
            }),
        )
            .into_response(),
        Err(error) => map_repo_error(error).response(request_id_value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_and_topics_are_bounded() {
        assert_eq!(clean_locale("pl"), Some("pl".to_owned()));
        assert_eq!(clean_locale("en-US"), Some("en-US".to_owned()));
        assert_eq!(clean_locale("PL"), None);
        assert_eq!(clean_locale("english"), None);
        assert_eq!(
            clean_topics(vec![
                "shows".into(),
                "press_materials".into(),
                "shows".into()
            ]),
            Some(vec!["press_materials".to_owned(), "shows".to_owned()])
        );
        assert_eq!(clean_topics(vec!["spam".into()]), None);
    }

    #[test]
    fn invite_token_shape_is_url_safe() {
        let token = random_token::<24>().expect("OS RNG should be available in tests");
        assert!(valid_invite_token(&token));
        assert!(!valid_invite_token("bad token with spaces"));
    }

    #[test]
    fn radius_is_intentionally_bounded() {
        assert!(valid_radius(10));
        assert!(valid_radius(100));
        assert!(valid_radius(500));
        assert!(!valid_radius(9));
        assert!(!valid_radius(501));
    }
}
