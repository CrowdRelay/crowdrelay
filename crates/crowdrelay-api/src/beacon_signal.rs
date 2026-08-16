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

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %beacon_id, "Beacon invite transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let beacon = sqlx::query_as::<_, (String, bool, bool, bool, bool)>(
        r#"
        SELECT display_name, active, verified, accepts_outreach, do_not_contact
        FROM viryaos_beacons
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(&mut *tx)
    .await;
    let (display_name, active, verified, accepts_outreach, do_not_contact) = match beacon {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, %beacon_id, "Beacon invite lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if !active || !verified || !accepts_outreach || do_not_contact {
        return BeaconSignalError::Conflict.response(request_id_value);
    }

    let existing_status = sqlx::query_scalar::<_, String>(
        r#"
        SELECT status
        FROM viryaos_beacon_signal_profiles
        WHERE workspace_id=$1 AND beacon_id=$2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(&mut *tx)
    .await;
    match existing_status {
        Ok(Some(status)) if status == "active" => {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, %beacon_id, "Beacon Signal profile state lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }

    let Some(invite_token) = random_token::<24>() else {
        return BeaconSignalError::Unavailable.response(request_id_value);
    };
    let expires_at = OffsetDateTime::now_utc() + Duration::days(payload.ttl_days);
    let profile_result = sqlx::query(
        r#"
        INSERT INTO viryaos_beacon_signal_profiles (
            workspace_id, beacon_id, status, invite_token_hash, invite_expires_at,
            radius_km, locale, nearby_gigs_enabled, invite_count, last_invited_at
        ) VALUES ($1, $2, 'invited', $3, $4, $5, $6, true, 1, now())
        ON CONFLICT (workspace_id, beacon_id) DO UPDATE SET
            status = 'invited',
            invite_token_hash = EXCLUDED.invite_token_hash,
            invite_expires_at = EXCLUDED.invite_expires_at,
            radius_km = EXCLUDED.radius_km,
            locale = EXCLUDED.locale,
            invite_count = viryaos_beacon_signal_profiles.invite_count + 1,
            last_invited_at = now(), paused_at = NULL, revoked_at = NULL,
            updated_at = now()
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(token_hash(&invite_token))
    .bind(expires_at)
    .bind(payload.radius_km)
    .bind(&locale)
    .execute(&mut *tx)
    .await;
    if let Err(error) = profile_result {
        tracing::warn!(%error, %beacon_id, "Beacon Signal invite persistence failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }

    // A re-invite is a new trust ceremony. Old device sessions must never become
    // valid again when this profile transitions back to `active` after exchange.
    let revoke_result = sqlx::query(
        r#"
        UPDATE viryaos_beacon_signal_sessions
        SET revoked_at=COALESCE(revoked_at, now())
        WHERE workspace_id=$1 AND beacon_id=$2 AND revoked_at IS NULL
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .execute(&mut *tx)
    .await;
    if let Err(error) = revoke_result {
        tracing::warn!(%error, %beacon_id, "Beacon Signal old-session revocation failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, %beacon_id, "Beacon Signal invite transaction failed to commit");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }

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
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!(%error, "Beacon Signal exchange transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            i32,
            String,
            Vec<String>,
            bool,
            OffsetDateTime,
        ),
    >(
        r#"
        SELECT profile.beacon_id, beacon.display_name, beacon.beacon_kind,
               profile.radius_km, profile.locale, profile.topics,
               profile.nearby_gigs_enabled, profile.invite_expires_at
        FROM viryaos_beacon_signal_profiles profile
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id = profile.workspace_id AND beacon.id = profile.beacon_id
        WHERE profile.workspace_id = $1
          AND profile.invite_token_hash = $2
          AND profile.status = 'invited'
          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact
        FOR UPDATE OF profile
        "#,
    )
    .bind(workspace_id)
    .bind(token_hash(payload.invite_token.trim()))
    .fetch_optional(&mut *tx)
    .await;
    let (
        beacon_id,
        display_name,
        beacon_kind,
        stored_radius,
        stored_locale,
        stored_topics,
        nearby_gigs_enabled,
        _,
    ) = match row {
        Ok(Some(row)) if row.7 >= OffsetDateTime::now_utc() => row,
        Ok(_) => return BeaconSignalError::Unauthorized.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Beacon Signal invite exchange lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let final_radius = payload.radius_km.unwrap_or(stored_radius);
    let final_locale = locale.unwrap_or(stored_locale);
    let final_topics = topics.unwrap_or(stored_topics);
    let Some(bearer_token) = random_token::<32>() else {
        return BeaconSignalError::Unavailable.response(request_id_value);
    };
    let session_id = Uuid::now_v7();
    let expires_at = OffsetDateTime::now_utc() + Duration::days(SESSION_TTL_DAYS);
    let profile_update = sqlx::query(
        r#"
        UPDATE viryaos_beacon_signal_profiles
        SET status='active', invite_token_hash=NULL, invite_expires_at=NULL,
            radius_km=$3, locale=$4, topics=$5,
            joined_at=COALESCE(joined_at, now()), last_seen_at=now(), updated_at=now()
        WHERE workspace_id=$1 AND beacon_id=$2
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(final_radius)
    .bind(&final_locale)
    .bind(&final_topics)
    .execute(&mut *tx)
    .await;
    let session_insert = sqlx::query(
        r#"
        INSERT INTO viryaos_beacon_signal_sessions
            (workspace_id, id, beacon_id, token_hash, expires_at)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(session_id)
    .bind(beacon_id)
    .bind(token_hash(&bearer_token))
    .bind(expires_at)
    .execute(&mut *tx)
    .await;
    if profile_update.is_err() || session_insert.is_err() || tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ExchangeInviteResponse {
            version: 1,
            role: "beacon",
            beacon_id,
            display_name,
            beacon_kind,
            bearer_token,
            session_id,
            expires_at,
            preferences: BeaconPreferences {
                radius_km: final_radius,
                locale: final_locale,
                topics: final_topics,
                nearby_gigs_enabled,
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
    let result = sqlx::query_as::<_, (i32, String, Vec<String>, bool)>(
        r#"
        UPDATE viryaos_beacon_signal_profiles
        SET radius_km=COALESCE($3,radius_km), locale=COALESCE($4,locale),
            topics=COALESCE($5,topics), nearby_gigs_enabled=COALESCE($6,nearby_gigs_enabled),
            updated_at=now()
        WHERE workspace_id=$1 AND beacon_id=$2 AND status='active'
        RETURNING radius_km, locale, topics, nearby_gigs_enabled
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(principal.beacon_id)
    .bind(payload.radius_km)
    .bind(locale)
    .bind(topics)
    .bind(payload.nearby_gigs_enabled)
    .fetch_optional(state.ticketing.pool())
    .await;
    match result {
        Ok(Some((radius_km, locale, topics, nearby_gigs_enabled))) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(BeaconPreferences {
                radius_km,
                locale,
                topics,
                nearby_gigs_enabled,
            }),
        )
            .into_response(),
        Ok(None) => BeaconSignalError::Unauthorized.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon preferences update failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
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
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Beacon press request transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if let Some(event_id) = payload.event_id {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM events WHERE workspace_id=$1 AND id=$2)",
        )
        .bind(workspace_id)
        .bind(event_id)
        .fetch_one(&mut *tx)
        .await;
        match exists {
            Ok(true) => {}
            Ok(false) => return BeaconSignalError::NotFound.response(request_id_value),
            Err(error) => {
                tracing::warn!(%error, %event_id, "Beacon press request event lookup failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
        }
    }
    let request_id_generated = Uuid::now_v7();
    if let Err(error) = sqlx::query(
        r#"
        INSERT INTO viryaos_beacon_press_requests
            (id,workspace_id,beacon_id,event_id,request_kind,details)
        VALUES ($1,$2,$3,$4,$5,$6)
        "#,
    )
    .bind(request_id_generated)
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(payload.event_id)
    .bind(payload.request_kind.as_str())
    .bind(&details)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon press request failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    let event_payload = serde_json::json!({
        "request_id": request_id_generated,
        "beacon_id": principal.beacon_id,
        "event_id": payload.event_id,
        "request_kind": payload.request_kind.as_str(),
        "details": details,
    });
    if let Err(error) = sqlx::query(
        "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'viryaos.beacon.press_request_created',1,$2,$3)",
    )
    .bind(workspace_id)
    .bind(event_payload)
    .bind(request_id(&headers))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon press request outbox write failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon press request transaction failed to commit");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(PressRequestResponse {
            request_id: request_id_generated,
            status: "open",
        }),
    )
        .into_response()
}

pub async fn logout(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    let session = sqlx::query(
        "UPDATE viryaos_beacon_signal_sessions SET revoked_at=now() WHERE workspace_id=$1 AND token_hash=$2 AND revoked_at IS NULL",
    )
    .bind(workspace_id)
    .bind(&principal.session_hash)
    .execute(&mut *tx)
    .await;
    let endpoint = sqlx::query(
        r#"
        UPDATE fan_push_endpoints
        SET active=false, invalidated_at=now(), updated_at=now()
        WHERE workspace_id=$1 AND audience_kind='beacon' AND principal_hash=$2 AND active
        "#,
    )
    .bind(workspace_id)
    .bind(&principal.session_hash)
    .execute(&mut *tx)
    .await;
    if session.is_err() || endpoint.is_err() || tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response()
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
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let push_enabled = if state.push.runtime_enabled {
        crate::ecosystem::feature_enabled(&state, "push_delivery_enabled")
            .await
            .unwrap_or(false)
    } else {
        false
    };
    let result = sqlx::query_as::<_, (i64, i64)>(
        r#"
        WITH candidates AS (
            SELECT beacon.id AS beacon_id,event.id AS event_id,
                   event.title AS event_title,event.starts_at,profile.locale,profile.radius_km,
                   beacon.relationship_score,beacon.relevance_basis_points,
                   engagement.last_notified_at,
                   LEAST(20000,ROUND(
                       6371 * 2 * ASIN(LEAST(1.0,SQRT(
                           POWER(SIN(RADIANS(home_city.latitude - event_city.latitude) / 2),2)
                           + COS(RADIANS(event_city.latitude)) * COS(RADIANS(home_city.latitude))
                           * POWER(SIN(RADIANS(home_city.longitude - event_city.longitude) / 2),2)
                       )))
                   )::integer) AS distance_km
            FROM viryaos_beacon_signal_profiles profile
            JOIN viryaos_beacons beacon
              ON beacon.workspace_id=profile.workspace_id AND beacon.id=profile.beacon_id
            JOIN cities home_city ON home_city.id=beacon.city_id
            JOIN events event ON event.workspace_id=profile.workspace_id
              AND event.status='published' AND event.starts_at > now()
              AND event.starts_at < now() + ($4::bigint * interval '1 day')
            JOIN cities event_city ON event_city.id=event.city_id
            LEFT JOIN viryaos_beacon_signal_event_engagements engagement
              ON engagement.workspace_id=profile.workspace_id
             AND engagement.beacon_id=beacon.id AND engagement.event_id=event.id
            LEFT JOIN viryaos_beacon_campaigns campaign
              ON campaign.workspace_id=profile.workspace_id
             AND campaign.beacon_id=beacon.id AND campaign.event_id=event.id
            WHERE profile.workspace_id=$1 AND profile.status='active'
              AND profile.nearby_gigs_enabled AND 'shows'=ANY(profile.topics)
              AND beacon.active AND beacon.verified AND beacon.accepts_outreach
              AND NOT beacon.do_not_contact
              AND home_city.latitude IS NOT NULL AND home_city.longitude IS NOT NULL
              AND event_city.latitude IS NOT NULL AND event_city.longitude IS NOT NULL
              AND COALESCE(engagement.status,'eligible') NOT IN ('completed','declined')
              AND engagement.last_notified_at IS NULL
              AND COALESCE(campaign.status,'candidate') NOT IN ('declined','suppressed','closed')
        ), ranked AS (
            SELECT * FROM candidates
            WHERE distance_km <= radius_km
            ORDER BY starts_at,relevance_basis_points DESC,relationship_score DESC,
                     distance_km,beacon_id,event_id
            LIMIT $2
        ), campaign_seed AS (
            INSERT INTO viryaos_beacon_campaigns (workspace_id,beacon_id,event_id,status)
            SELECT $1,beacon_id,event_id,'candidate' FROM ranked
            ON CONFLICT (workspace_id,beacon_id,event_id) DO NOTHING
            RETURNING beacon_id,event_id
        ), engagement_seed AS (
            INSERT INTO viryaos_beacon_signal_event_engagements
                (workspace_id,beacon_id,event_id,status)
            SELECT $1,beacon_id,event_id,'eligible' FROM ranked
            ON CONFLICT (workspace_id,beacon_id,event_id) DO UPDATE SET
                updated_at=viryaos_beacon_signal_event_engagements.updated_at
            RETURNING beacon_id,event_id
        ), push_queued AS (
            INSERT INTO fan_push_deliveries (
                workspace_id,fan_id,audience_kind,endpoint_id,source_kind,source_id,
                title,body,target_path,collapse_key
            )
            SELECT $1,NULL,'beacon',endpoint.id,'beacon_nearby_concert',ranked.event_id,
                   CASE WHEN lower(ranked.locale) LIKE 'pl%'
                        THEN 'VIRYA · materiał lokalny' ELSE 'VIRYA · local story' END,
                   CASE WHEN lower(ranked.locale) LIKE 'pl%'
                        THEN ranked.event_title || ' — gramy około ' || ranked.distance_km || ' km od Ciebie. Press room jest gotowy.'
                        ELSE ranked.event_title || ' — we play about ' || ranked.distance_km || ' km from you. The press room is ready.' END,
                   CASE WHEN lower(ranked.locale) LIKE 'pl%'
                        THEN '/pl/latarnik?event_id=' || ranked.event_id::text
                        ELSE '/latarnik?event_id=' || ranked.event_id::text END,
                   'beacon-nearby:' || ranked.event_id::text
            FROM ranked
            JOIN engagement_seed seeded
              ON seeded.beacon_id=ranked.beacon_id AND seeded.event_id=ranked.event_id
            JOIN viryaos_beacon_signal_sessions session
              ON session.workspace_id=$1 AND session.beacon_id=ranked.beacon_id
             AND session.revoked_at IS NULL AND session.expires_at > now()
            JOIN fan_push_endpoints endpoint
              ON endpoint.workspace_id=$1 AND endpoint.audience_kind='beacon'
             AND endpoint.principal_hash=session.token_hash
             AND endpoint.active AND endpoint.invalidated_at IS NULL
            WHERE $3::boolean
            ON CONFLICT (workspace_id,source_kind,source_id,endpoint_id) DO NOTHING
            RETURNING endpoint_id,source_id
        ), notified_pairs AS (
            SELECT DISTINCT session.beacon_id,push_queued.source_id AS event_id
            FROM push_queued
            JOIN fan_push_endpoints endpoint
              ON endpoint.workspace_id=$1 AND endpoint.id=push_queued.endpoint_id
            JOIN viryaos_beacon_signal_sessions session
              ON session.workspace_id=$1 AND session.token_hash=endpoint.principal_hash
        ), marked AS (
            UPDATE viryaos_beacon_signal_event_engagements engagement
            SET status=CASE WHEN engagement.status='eligible' THEN 'notified' ELSE engagement.status END,
                notification_count=engagement.notification_count + 1,
                first_notified_at=COALESCE(engagement.first_notified_at,now()),
                last_notified_at=now(),updated_at=now()
            FROM notified_pairs notified
            WHERE engagement.workspace_id=$1
              AND engagement.beacon_id=notified.beacon_id AND engagement.event_id=notified.event_id
            RETURNING engagement.beacon_id,engagement.event_id
        ), campaign_contacted AS (
            UPDATE viryaos_beacon_campaigns campaign
            SET status=CASE WHEN campaign.status='candidate' THEN 'contacted' ELSE campaign.status END,
                last_phase='local_push',last_outreach_at=now(),updated_at=now()
            FROM marked
            WHERE campaign.workspace_id=$1
              AND campaign.beacon_id=marked.beacon_id AND campaign.event_id=marked.event_id
              AND campaign.status NOT IN ('declined','suppressed','closed')
            RETURNING campaign.beacon_id,campaign.event_id
        )
        SELECT (SELECT count(*)::bigint FROM ranked),
               (SELECT count(*)::bigint FROM push_queued)
        "#,
    )
    .bind(workspace_id)
    .bind(payload.limit)
    .bind(push_enabled)
    .bind(payload.lead_days)
    .fetch_one(state.ticketing.pool())
    .await;
    match result {
        Ok((eligible, push_queued)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(EmitNearbyResponse {
                eligible,
                push_queued,
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "Beacon nearby notification wave failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
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
