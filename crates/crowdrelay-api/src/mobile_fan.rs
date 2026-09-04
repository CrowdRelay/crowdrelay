//! Mobile onboarding endpoints for moderated cities and nearby-gig delivery.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_infra::mobile_fan::{
    CityRequestCommand, MobileFanStoreError, PostgresMobileFanRepository,
};

use crate::{IDEMPOTENCY_KEY, Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCity {
    name: String,
    region: Option<String>,
    country_code: String,
}

#[derive(Debug, Serialize)]
pub struct RequestedCity {
    city_slug: String,
    display_name: String,
    status: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeocodeCity {
    latitude: f64,
    longitude: f64,
    canonical_name: Option<String>,
    region: Option<String>,
    /// Also promote a fan-requested city out of `pending`. Coordinates alone
    /// make it reachable by proximity; approval is what lets it appear in the
    /// city signal list and host an AREA drop. Defaults off so geocoding stays
    /// separable from the moderation decision.
    #[serde(default)]
    approve: bool,
}

fn clean(value: &str, max_chars: usize) -> Option<String> {
    if value.chars().any(char::is_control) {
        return None;
    }
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!value.is_empty() && value.chars().count() <= max_chars).then_some(value)
}

fn requested_slug(name: &str, country: &str) -> String {
    let normalized = name
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    let suffix = hex::encode(Sha256::digest(format!("{country}\0{name}")))
        .chars()
        .take(10)
        .collect::<String>();
    let label = if normalized.is_empty() {
        "city"
    } else {
        normalized.as_str()
    };
    format!("pending-{label}-{suffix}")
}

fn mobile_fan_repository(state: &crate::AppState) -> PostgresMobileFanRepository {
    PostgresMobileFanRepository::new(
        state.ticketing.pool().clone(),
        state.ticketing.workspace_id(),
        state.ticketing.operation_timeout(),
    )
}

pub async fn request_city(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RequestCity>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let idempotency_key = match headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(IdempotencyKey::parse)
    {
        Some(Ok(value)) => value,
        _ => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(name) = clean(&payload.name, 120) else {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    };
    let region = payload
        .region
        .as_deref()
        .and_then(|value| clean(value, 120));
    let country = payload.country_code.trim().to_ascii_uppercase();
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }
    let slug = requested_slug(&name, &country);
    let command = CityRequestCommand {
        idempotency_key,
        request_id: request_id_value.clone(),
        name,
        region,
        country_code: country,
        slug,
    };
    match mobile_fan_repository(&state).request_city(&command).await {
        Ok(result) => {
            let status = if result.status == "approved" {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (
                status,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(RequestedCity {
                    city_slug: result.city_slug,
                    display_name: result.display_name,
                    status: result.status,
                }),
            )
                .into_response()
        }
        Err(MobileFanStoreError::Conflict) => Problem::conflict(request_id_value)
            .private()
            .into_response(),
        Err(MobileFanStoreError::Unavailable) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn geocode_city(
    State(state): State<crate::AppState>,
    Path(city_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<GeocodeCity>, JsonRejection>,
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
    if !payload.latitude.is_finite()
        || !payload.longitude.is_finite()
        || !(-90.0..=90.0).contains(&payload.latitude)
        || !(-180.0..=180.0).contains(&payload.longitude)
    {
        return Problem::unprocessable(request_id_value)
            .private()
            .into_response();
    }
    let canonical_name = payload
        .canonical_name
        .as_deref()
        .and_then(|value| clean(value, 120));
    let region = payload
        .region
        .as_deref()
        .and_then(|value| clean(value, 120));
    match mobile_fan_repository(&state)
        .geocode_city(
            city_id,
            payload.latitude,
            payload.longitude,
            canonical_name,
            region,
            payload.approve,
        )
        .await
    {
        Ok(true) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({"updated": true, "city_id": city_id})),
        )
            .into_response(),
        Ok(false) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

/// The queue behind the ops snapshot's `pending_city_requests` counter: which
/// cities fans asked for, and how many of them are waiting in each.
pub async fn pending_cities(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    match mobile_fan_repository(&state).list_pending_cities(100).await {
        Ok(items) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({"items": items})),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "could not list cities awaiting coordinates");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

pub async fn emit_due_nearby_gigs(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.commerce_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let push_enabled = if state.push.runtime_enabled {
        crate::ecosystem::feature_enabled(&state, "push_delivery_enabled")
            .await
            .unwrap_or(false)
    } else {
        false
    };
    match mobile_fan_repository(&state)
        .emit_due_nearby_gigs(request_id_value.as_deref(), push_enabled)
        .await
    {
        Ok((event_count, push_count)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({"queued": event_count, "push_queued": push_count})),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "could not emit due nearby-gig notifications");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_city_slug_is_stable_and_bounded() {
        let first = requested_slug("Bielawa", "PL");
        let second = requested_slug("Bielawa", "PL");
        assert_eq!(first, second);
        assert!(first.starts_with("pending-bielawa-"));
        assert!(first.len() <= 128);
    }

    #[test]
    fn city_text_is_trimmed_and_rejects_control_characters() {
        assert_eq!(clean(" Bielawa ", 120), Some("Bielawa".to_owned()));
        assert_eq!(
            clean("  Wrocław   dolnośląskie  ", 120),
            Some("Wrocław dolnośląskie".to_owned())
        );
        assert_eq!(clean("Bielawa\nInjected", 120), None);
    }
}
