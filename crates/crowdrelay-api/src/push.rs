//! Fan push endpoint registration and device acknowledgement.
//!
//! Registration is fan-session authenticated and endpoint material is write-only.
//! A provider 2xx is intentionally not called "delivered": the terminal delivered
//! state is written only by the short-lived acknowledgement capability embedded
//! in the encrypted push payload after the client displays the notification.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::{Problem, acquisition::fan_session_from_headers, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_INSTALLATION_ID: usize = 160;
const MAX_ENDPOINT_ADDRESS: usize = 4096;
const MAX_PUSH_KEY: usize = 256;

#[derive(Debug, Clone)]
pub struct PushPublicState {
    pub runtime_enabled: bool,
    pub web_push_vapid_public_key: Option<String>,
    pub fcm_project_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PushConfigResponse {
    enabled: bool,
    android_fcm: bool,
    web_push: bool,
    vapid_public_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterPushEndpointRequest {
    installation_id: String,
    transport: String,
    endpoint: String,
    p256dh: Option<String>,
    auth: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisablePushEndpointRequest {
    installation_id: String,
    transport: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushDeliveryAckRequest {
    ack_token: String,
}

#[derive(Debug, Serialize)]
pub struct PushEndpointMutationResponse {
    registered: bool,
}

#[derive(Debug, Serialize)]
pub struct PushDeliveryAckResponse {
    status: &'static str,
}

#[derive(Debug)]
enum PushError {
    Unauthorized,
    BadRequest,
    Conflict,
    NotFound,
    Unavailable,
}

impl PushError {
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

pub async fn config(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let db_enabled = match crate::ecosystem::feature_enabled(&state, "push_delivery_enabled").await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not read push delivery feature flag");
            return PushError::Unavailable.response(request_id_value);
        }
    };
    let enabled = state.push.runtime_enabled && db_enabled;
    let web_push = enabled && state.push.web_push_vapid_public_key.is_some();
    let android_fcm = enabled && state.push.fcm_project_id.is_some();
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(PushConfigResponse {
            enabled,
            android_fcm,
            web_push,
            vapid_public_key: web_push
                .then(|| state.push.web_push_vapid_public_key.clone())
                .flatten(),
        }),
    )
        .into_response()
}

pub async fn register_endpoint(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RegisterPushEndpointRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return PushError::BadRequest.response(request_id_value),
    };
    let fan_id = match current_fan_id(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    if !push_enabled(&state).await.unwrap_or(false) {
        return PushError::Conflict.response(request_id_value);
    }
    if !marketing_consented(&state, fan_id).await.unwrap_or(false) {
        return PushError::Conflict.response(request_id_value);
    }

    let installation_id = payload.installation_id.trim();
    let transport = payload.transport.trim();
    let endpoint = payload.endpoint.trim();
    if !valid_installation_id(installation_id)
        || endpoint.is_empty()
        || endpoint.len() > MAX_ENDPOINT_ADDRESS
    {
        return PushError::BadRequest.response(request_id_value);
    }
    let (p256dh, auth) = match transport {
        "android_fcm" if state.push.fcm_project_id.is_some() => {
            if !valid_fcm_token(endpoint) || payload.p256dh.is_some() || payload.auth.is_some() {
                return PushError::BadRequest.response(request_id_value);
            }
            (None, None)
        }
        "web_push" if state.push.web_push_vapid_public_key.is_some() => {
            if !valid_web_push_endpoint(endpoint) {
                return PushError::BadRequest.response(request_id_value);
            }
            let Some(p256dh) = payload.p256dh.as_deref().map(str::trim) else {
                return PushError::BadRequest.response(request_id_value);
            };
            let Some(auth) = payload.auth.as_deref().map(str::trim) else {
                return PushError::BadRequest.response(request_id_value);
            };
            if !valid_push_key(p256dh, 40) || !valid_push_key(auth, 8) {
                return PushError::BadRequest.response(request_id_value);
            }
            (Some(p256dh), Some(auth))
        }
        _ => return PushError::BadRequest.response(request_id_value),
    };

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let result = sqlx::query(
        r#"
        INSERT INTO fan_push_endpoints (
            workspace_id, fan_id, audience_kind, principal_hash,
            installation_id, transport, endpoint_address, p256dh, auth_secret,
            active, last_seen_at, invalidated_at, last_error_code
        )
        VALUES ($1, $2, 'fan', NULL, $3, $4, $5, $6, $7, true, now(), NULL, NULL)
        ON CONFLICT (workspace_id, installation_id, transport, audience_kind)
        DO UPDATE SET
            fan_id = EXCLUDED.fan_id,
            endpoint_address = EXCLUDED.endpoint_address,
            p256dh = EXCLUDED.p256dh,
            auth_secret = EXCLUDED.auth_secret,
            active = true,
            last_seen_at = now(),
            invalidated_at = NULL,
            last_error_code = NULL,
            updated_at = now()
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(installation_id)
    .bind(transport)
    .bind(endpoint)
    .bind(p256dh)
    .bind(auth)
    .execute(&state.database)
    .await;
    match result {
        Ok(_) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PushEndpointMutationResponse { registered: true }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, %fan_id, transport, "could not register fan push endpoint");
            PushError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn disable_endpoint(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<DisablePushEndpointRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return PushError::BadRequest.response(request_id_value),
    };
    let fan_id = match current_fan_id(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let installation_id = payload.installation_id.trim();
    let transport = payload.transport.trim();
    if !valid_installation_id(installation_id) || !matches!(transport, "android_fcm" | "web_push") {
        return PushError::BadRequest.response(request_id_value);
    }
    let result = sqlx::query(
        r#"
        UPDATE fan_push_endpoints
        SET active = false, invalidated_at = now(), updated_at = now()
        WHERE workspace_id = $1 AND fan_id = $2 AND audience_kind = 'fan'
          AND installation_id = $3 AND transport = $4 AND active
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(fan_id)
    .bind(installation_id)
    .bind(transport)
    .execute(&state.database)
    .await;
    match result {
        Ok(_) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PushEndpointMutationResponse { registered: false }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, %fan_id, transport, "could not disable fan push endpoint");
            PushError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn register_staff_endpoint(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RegisterPushEndpointRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return PushError::BadRequest.response(request_id_value),
    };
    let Some(principal_hash) = crate::security::bearer_sha256(&headers) else {
        return PushError::Unauthorized.response(request_id_value);
    };
    if !push_enabled(&state).await.unwrap_or(false) {
        return PushError::Conflict.response(request_id_value);
    }
    let installation_id = payload.installation_id.trim();
    let transport = payload.transport.trim();
    let endpoint = payload.endpoint.trim();
    if !valid_installation_id(installation_id)
        || endpoint.is_empty()
        || endpoint.len() > MAX_ENDPOINT_ADDRESS
    {
        return PushError::BadRequest.response(request_id_value);
    }
    let (p256dh, auth) = match transport {
        "android_fcm" if state.push.fcm_project_id.is_some() => {
            if !valid_fcm_token(endpoint) || payload.p256dh.is_some() || payload.auth.is_some() {
                return PushError::BadRequest.response(request_id_value);
            }
            (None, None)
        }
        "web_push" if state.push.web_push_vapid_public_key.is_some() => {
            if !valid_web_push_endpoint(endpoint) {
                return PushError::BadRequest.response(request_id_value);
            }
            let Some(p256dh) = payload.p256dh.as_deref().map(str::trim) else {
                return PushError::BadRequest.response(request_id_value);
            };
            let Some(auth) = payload.auth.as_deref().map(str::trim) else {
                return PushError::BadRequest.response(request_id_value);
            };
            if !valid_push_key(p256dh, 40) || !valid_push_key(auth, 8) {
                return PushError::BadRequest.response(request_id_value);
            }
            (Some(p256dh), Some(auth))
        }
        _ => return PushError::BadRequest.response(request_id_value),
    };
    let result = sqlx::query(
        r#"
        INSERT INTO fan_push_endpoints (
            workspace_id, fan_id, audience_kind, principal_hash,
            installation_id, transport, endpoint_address, p256dh, auth_secret,
            active, last_seen_at, invalidated_at, last_error_code
        )
        VALUES ($1, NULL, 'staff', $2, $3, $4, $5, $6, $7, true, now(), NULL, NULL)
        ON CONFLICT (workspace_id, installation_id, transport, audience_kind)
        DO UPDATE SET
            fan_id = NULL,
            principal_hash = EXCLUDED.principal_hash,
            endpoint_address = EXCLUDED.endpoint_address,
            p256dh = EXCLUDED.p256dh,
            auth_secret = EXCLUDED.auth_secret,
            active = true,
            last_seen_at = now(),
            invalidated_at = NULL,
            last_error_code = NULL,
            updated_at = now()
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(principal_hash.to_vec())
    .bind(installation_id)
    .bind(transport)
    .bind(endpoint)
    .bind(p256dh)
    .bind(auth)
    .execute(&state.database)
    .await;
    match result {
        Ok(_) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PushEndpointMutationResponse { registered: true }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, transport, "could not register staff push endpoint");
            PushError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn disable_staff_endpoint(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<DisablePushEndpointRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return PushError::BadRequest.response(request_id_value),
    };
    let Some(principal_hash) = crate::security::bearer_sha256(&headers) else {
        return PushError::Unauthorized.response(request_id_value);
    };
    let installation_id = payload.installation_id.trim();
    let transport = payload.transport.trim();
    if !valid_installation_id(installation_id) || !matches!(transport, "android_fcm" | "web_push") {
        return PushError::BadRequest.response(request_id_value);
    }
    let result = sqlx::query(
        r#"
        UPDATE fan_push_endpoints
        SET active = false, invalidated_at = now(), updated_at = now()
        WHERE workspace_id = $1 AND audience_kind = 'staff'
          AND principal_hash = $2
          AND installation_id = $3 AND transport = $4 AND active
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(principal_hash.to_vec())
    .bind(installation_id)
    .bind(transport)
    .execute(&state.database)
    .await;
    match result {
        Ok(_) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PushEndpointMutationResponse { registered: false }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, transport, "could not disable staff push endpoint");
            PushError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn register_beacon_endpoint(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RegisterPushEndpointRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return PushError::BadRequest.response(request_id_value),
    };
    let principal = match crate::beacon_signal::authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(_) => return PushError::Unauthorized.response(request_id_value),
    };
    if !push_enabled(&state).await.unwrap_or(false) {
        return PushError::Conflict.response(request_id_value);
    }
    let installation_id = payload.installation_id.trim();
    let transport = payload.transport.trim();
    let endpoint = payload.endpoint.trim();
    if !valid_installation_id(installation_id)
        || endpoint.is_empty()
        || endpoint.len() > MAX_ENDPOINT_ADDRESS
    {
        return PushError::BadRequest.response(request_id_value);
    }
    let (p256dh, auth) = match transport {
        "android_fcm" if state.push.fcm_project_id.is_some() => {
            if !valid_fcm_token(endpoint) || payload.p256dh.is_some() || payload.auth.is_some() {
                return PushError::BadRequest.response(request_id_value);
            }
            (None, None)
        }
        "web_push" if state.push.web_push_vapid_public_key.is_some() => {
            if !valid_web_push_endpoint(endpoint) {
                return PushError::BadRequest.response(request_id_value);
            }
            let Some(p256dh) = payload.p256dh.as_deref().map(str::trim) else {
                return PushError::BadRequest.response(request_id_value);
            };
            let Some(auth) = payload.auth.as_deref().map(str::trim) else {
                return PushError::BadRequest.response(request_id_value);
            };
            if !valid_push_key(p256dh, 40) || !valid_push_key(auth, 8) {
                return PushError::BadRequest.response(request_id_value);
            }
            (Some(p256dh), Some(auth))
        }
        _ => return PushError::BadRequest.response(request_id_value),
    };
    let result = sqlx::query(
        r#"
        INSERT INTO fan_push_endpoints (
            workspace_id, fan_id, audience_kind, principal_hash,
            installation_id, transport, endpoint_address, p256dh, auth_secret,
            active, last_seen_at, invalidated_at, last_error_code
        )
        VALUES ($1, NULL, 'beacon', $2, $3, $4, $5, $6, $7, true, now(), NULL, NULL)
        ON CONFLICT (workspace_id, installation_id, transport, audience_kind)
        DO UPDATE SET
            fan_id = NULL,
            principal_hash = EXCLUDED.principal_hash,
            endpoint_address = EXCLUDED.endpoint_address,
            p256dh = EXCLUDED.p256dh,
            auth_secret = EXCLUDED.auth_secret,
            active = true,
            last_seen_at = now(),
            invalidated_at = NULL,
            last_error_code = NULL,
            updated_at = now()
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(&principal.session_hash)
    .bind(installation_id)
    .bind(transport)
    .bind(endpoint)
    .bind(p256dh)
    .bind(auth)
    .execute(&state.database)
    .await;
    match result {
        Ok(_) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PushEndpointMutationResponse { registered: true }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, transport, "could not register Beacon push endpoint");
            PushError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn disable_beacon_endpoint(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<DisablePushEndpointRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return PushError::BadRequest.response(request_id_value),
    };
    let principal = match crate::beacon_signal::authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(_) => return PushError::Unauthorized.response(request_id_value),
    };
    let installation_id = payload.installation_id.trim();
    let transport = payload.transport.trim();
    if !valid_installation_id(installation_id) || !matches!(transport, "android_fcm" | "web_push") {
        return PushError::BadRequest.response(request_id_value);
    }
    let result = sqlx::query(
        r#"
        UPDATE fan_push_endpoints
        SET active = false, invalidated_at = now(), updated_at = now()
        WHERE workspace_id = $1 AND audience_kind = 'beacon'
          AND principal_hash = $2
          AND installation_id = $3 AND transport = $4 AND active
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(&principal.session_hash)
    .bind(installation_id)
    .bind(transport)
    .execute(&state.database)
    .await;
    match result {
        Ok(_) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PushEndpointMutationResponse { registered: false }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, transport, "could not disable Beacon push endpoint");
            PushError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn acknowledge_delivery(
    State(state): State<crate::AppState>,
    Path(delivery_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<PushDeliveryAckRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return PushError::BadRequest.response(request_id_value),
    };
    let token = payload.ack_token.trim();
    if !(32..=200).contains(&token.len()) || !token.is_ascii() {
        return PushError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let updated = sqlx::query_scalar::<_, String>(
        r#"
        UPDATE fan_push_deliveries
        SET status = 'delivered', delivered_at = now(), completed_at = now(),
            error_code = NULL, updated_at = now()
        WHERE workspace_id = $1 AND id = $2
          AND ack_token_hash = digest($3, 'sha256')
          AND status IN ('provider_started','provider_accepted')
          AND ack_deadline > now()
        RETURNING status
        "#,
    )
    .bind(workspace_id)
    .bind(delivery_id)
    .bind(token)
    .fetch_optional(&state.database)
    .await;
    match updated {
        Ok(Some(_)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(PushDeliveryAckResponse {
                status: "delivered",
            }),
        )
            .into_response(),
        Ok(None) => {
            let existing = sqlx::query_scalar::<_, String>(
                r#"
                SELECT status FROM fan_push_deliveries
                WHERE workspace_id = $1 AND id = $2
                  AND ack_token_hash = digest($3, 'sha256')
                "#,
            )
            .bind(workspace_id)
            .bind(delivery_id)
            .bind(token)
            .fetch_optional(&state.database)
            .await;
            match existing {
                Ok(Some(status)) if status == "delivered" => (
                    StatusCode::OK,
                    [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                    Json(PushDeliveryAckResponse {
                        status: "delivered",
                    }),
                )
                    .into_response(),
                Ok(Some(_)) => PushError::Conflict.response(request_id_value),
                Ok(None) => PushError::NotFound.response(request_id_value),
                Err(error) => {
                    tracing::warn!(%error, %delivery_id, "could not verify push delivery acknowledgement");
                    PushError::Unavailable.response(request_id_value)
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, %delivery_id, "could not acknowledge push delivery");
            PushError::Unavailable.response(request_id_value)
        }
    }
}

async fn current_fan_id(state: &crate::AppState, headers: &HeaderMap) -> Result<Uuid, PushError> {
    let Some(session) = fan_session_from_headers(headers) else {
        return Err(PushError::Unauthorized);
    };
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT session.fan_id
        FROM fan_sessions session
        JOIN fans fan ON fan.workspace_id = session.workspace_id AND fan.id = session.fan_id
        WHERE session.workspace_id = $1
          AND session.session_token_hash = digest($2, 'sha256')
          AND session.revoked_at IS NULL
          AND session.expires_at > now()
          AND fan.status = 'active'
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(session.as_str())
    .fetch_optional(&state.database)
    .await
    .map_err(|error| {
        tracing::warn!(%error, "push fan-session lookup failed");
        PushError::Unavailable
    })?
    .ok_or(PushError::Unauthorized)
}

async fn marketing_consented(state: &crate::AppState, fan_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT COALESCE((
            SELECT granted
            FROM fan_consents
            WHERE workspace_id = $1 AND fan_id = $2 AND purpose = 'marketing'
            ORDER BY recorded_at DESC, id DESC
            LIMIT 1
        ), false)
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(fan_id)
    .fetch_one(&state.database)
    .await
}

async fn push_enabled(state: &crate::AppState) -> Result<bool, crate::ecosystem::EcosystemError> {
    if !state.push.runtime_enabled {
        return Ok(false);
    }
    crate::ecosystem::feature_enabled(state, "push_delivery_enabled").await
}

fn valid_installation_id(value: &str) -> bool {
    (8..=MAX_INSTALLATION_ID).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn valid_fcm_token(value: &str) -> bool {
    (32..=4096).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn valid_push_key(value: &str, min: usize) -> bool {
    (min..=MAX_PUSH_KEY).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'='))
}

fn valid_web_push_endpoint(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) else {
        return false;
    };
    host == "fcm.googleapis.com"
        || host.ends_with(".fcm.googleapis.com")
        || host == "updates.push.services.mozilla.com"
        || host.ends_with(".push.services.mozilla.com")
        || host == "web.push.apple.com"
        || host.ends_with(".push.apple.com")
        || host.ends_with(".notify.windows.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_push_endpoint_is_https_and_not_generic_ssrf() {
        assert!(valid_web_push_endpoint(
            "https://fcm.googleapis.com/fcm/send/token"
        ));
        assert!(valid_web_push_endpoint(
            "https://updates.push.services.mozilla.com/wpush/v2/token"
        ));
        assert!(valid_web_push_endpoint("https://web.push.apple.com/Q123"));
        assert!(!valid_web_push_endpoint(
            "http://fcm.googleapis.com/fcm/send/token"
        ));
        assert!(!valid_web_push_endpoint("https://127.0.0.1/push"));
        assert!(!valid_web_push_endpoint("https://example.com/push"));
    }

    #[test]
    fn installation_id_is_bounded_and_portable() {
        assert!(valid_installation_id("signal-12345678"));
        assert!(!valid_installation_id("short"));
        assert!(!valid_installation_id("bad id with spaces"));
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FanPushPreferencesResponse {
    shows: bool,
    releases: bool,
    community: bool,
    merch: bool,
    quiet_hours_enabled: bool,
    quiet_start: String,
    quiet_end: String,
    quiet_timezone: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateFanPushPreferencesRequest {
    shows: bool,
    releases: bool,
    community: bool,
    merch: bool,
    quiet_hours_enabled: bool,
    quiet_start: String,
    quiet_end: String,
}

fn minute_of_day(value: &str) -> Option<i16> {
    let (hours, minutes) = value.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours = hours.parse::<i16>().ok()?;
    let minutes = minutes.parse::<i16>().ok()?;
    if !(0..24).contains(&hours) || !(0..60).contains(&minutes) {
        return None;
    }
    Some(hours * 60 + minutes)
}

fn minute_text(value: i16) -> String {
    format!("{:02}:{:02}", value / 60, value % 60)
}

pub async fn fan_preferences(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let fan_id = match current_fan_id(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let row = sqlx::query_as::<_, (bool, bool, bool, bool, bool, i16, i16)>(
        r#"
        SELECT shows_enabled, releases_enabled, community_enabled, merch_enabled,
               quiet_hours_enabled, quiet_start_minute, quiet_end_minute
        FROM fan_push_preferences
        WHERE workspace_id = $1 AND fan_id = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(fan_id)
    .fetch_optional(&state.database)
    .await;
    match row {
        Ok(row) => {
            let (shows, releases, community, merch, quiet, start, end) =
                row.unwrap_or((true, true, true, true, false, 1320, 480));
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(FanPushPreferencesResponse {
                    shows,
                    releases,
                    community,
                    merch,
                    quiet_hours_enabled: quiet,
                    quiet_start: minute_text(start),
                    quiet_end: minute_text(end),
                    quiet_timezone: state.tenant.regional.timezone.clone(),
                }),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not read fan push preferences");
            PushError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn update_fan_preferences(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<UpdateFanPushPreferencesRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return PushError::BadRequest.response(request_id_value),
    };
    let fan_id = match current_fan_id(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Some(start) = minute_of_day(&payload.quiet_start) else {
        return PushError::BadRequest.response(request_id_value);
    };
    let Some(end) = minute_of_day(&payload.quiet_end) else {
        return PushError::BadRequest.response(request_id_value);
    };
    let value = crowdrelay_infra::push_preferences::FanPushPreferencesUpdate {
        shows_enabled: payload.shows,
        releases_enabled: payload.releases,
        community_enabled: payload.community,
        merch_enabled: payload.merch,
        quiet_hours_enabled: payload.quiet_hours_enabled,
        quiet_start_minute: start,
        quiet_end_minute: end,
    };
    match crowdrelay_infra::push_preferences::upsert_fan_push_preferences(
        &state.database,
        state.ticketing.workspace_id().into_uuid(),
        fan_id,
        value,
    )
    .await
    {
        Ok(()) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(FanPushPreferencesResponse {
                shows: payload.shows,
                releases: payload.releases,
                community: payload.community,
                merch: payload.merch,
                quiet_hours_enabled: payload.quiet_hours_enabled,
                quiet_start: minute_text(start),
                quiet_end: minute_text(end),
                quiet_timezone: state.tenant.regional.timezone.clone(),
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not update fan push preferences");
            PushError::Unavailable.response(request_id_value)
        }
    }
}
