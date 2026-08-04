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

use crate::{IDEMPOTENCY_KEY, Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const CITY_NOTIFICATION_COOLDOWN_MINUTES: i64 = 60;

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
    status: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeocodeCity {
    latitude: f64,
    longitude: f64,
    canonical_name: Option<String>,
    region: Option<String>,
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

pub async fn request_city(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RequestCity>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if headers.get(&IDEMPOTENCY_KEY).is_none() {
        return Problem::bad_request(request_id_value)
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
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let approved = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT slug, name
        FROM cities
        WHERE country_code = $1
          AND moderation_status = 'approved'
          AND lower(btrim(name)) = lower($2)
        ORDER BY
            CASE
                WHEN $3::text IS NOT NULL
                 AND region IS NOT NULL
                 AND lower(btrim(region)) = lower($3)
                THEN 0
                ELSE 1
            END,
            id
        LIMIT 1
        "#,
    )
    .bind(&country)
    .bind(&name)
    .bind(region.as_deref())
    .fetch_optional(&mut *transaction)
    .await;
    match approved {
        Ok(Some((city_slug, display_name))) => {
            if transaction.commit().await.is_err() {
                return Problem::service_unavailable(request_id_value)
                    .private()
                    .into_response();
            }
            return (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(RequestedCity {
                    city_slug,
                    display_name,
                    status: "approved",
                }),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(_) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    }

    let row = sqlx::query_as::<_, (Uuid, String, String, i32)>(
        r#"
        INSERT INTO cities (
            slug, name, country_code, region, moderation_status,
            request_count, first_requested_at, last_requested_at
        )
        VALUES ($1, $2, $3, $4, 'pending', 1, now(), now())
        ON CONFLICT (country_code, slug) DO UPDATE
        SET request_count = cities.request_count + 1,
            last_requested_at = now(),
            region = COALESCE(cities.region, EXCLUDED.region)
        RETURNING id, slug, name, request_count
        "#,
    )
    .bind(&slug)
    .bind(&name)
    .bind(&country)
    .bind(region.as_deref())
    .fetch_one(&mut *transaction)
    .await;
    let Ok((city_id, city_slug, display_name, request_count)) = row else {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    };

    let queued = sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id
        )
        SELECT
            $1,
            'fan.city_requested',
            1,
            jsonb_build_object(
                'city_id', $2::uuid,
                'city_slug', $3::text,
                'display_name', $4::text,
                'country_code', $5::text,
                'request_count', $6::integer
            ),
            $7
        WHERE NOT EXISTS (
            SELECT 1
            FROM outbox_events
            WHERE workspace_id = $1
              AND event_type = 'fan.city_requested'
              AND payload ->> 'city_slug' = $3
              AND created_at > now() - ($8::bigint * interval '1 minute')
        )
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(city_id)
    .bind(&city_slug)
    .bind(&display_name)
    .bind(&country)
    .bind(request_count)
    .bind(request_id_value.as_deref())
    .bind(CITY_NOTIFICATION_COOLDOWN_MINUTES)
    .execute(&mut *transaction)
    .await;
    if queued.is_err() || transaction.commit().await.is_err() {
        return Problem::service_unavailable(request_id_value)
            .private()
            .into_response();
    }

    (
        StatusCode::ACCEPTED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(RequestedCity {
            city_slug,
            display_name,
            status: "pending",
        }),
    )
        .into_response()
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
    let updated = sqlx::query(
        r#"
        UPDATE cities
        SET latitude = $2,
            longitude = $3,
            name = COALESCE($4, name),
            region = COALESCE($5, region)
        WHERE id = $1
          AND moderation_status IN ('pending', 'approved')
        "#,
    )
    .bind(city_id)
    .bind(payload.latitude)
    .bind(payload.longitude)
    .bind(canonical_name)
    .bind(region)
    .execute(state.ticketing.pool())
    .await;
    match updated {
        Ok(result) if result.rows_affected() == 1 => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({"updated": true, "city_id": city_id})),
        )
            .into_response(),
        Ok(_) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
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
    let queued = sqlx::query_scalar::<_, i64>(
        r#"
        WITH candidates AS (
            SELECT
                fans.id AS fan_id,
                fans.normalized_email,
                events.id AS event_id,
                events.slug AS event_slug,
                events.title AS event_title,
                events.starts_at,
                events.venue,
                event_city.name AS event_city,
                LEAST(
                    20000,
                    ROUND(
                        6371 * 2 * ASIN(LEAST(1.0, SQRT(
                            POWER(SIN(RADIANS(fan_city.latitude - event_city.latitude) / 2), 2)
                            + COS(RADIANS(event_city.latitude))
                            * COS(RADIANS(fan_city.latitude))
                            * POWER(SIN(RADIANS(fan_city.longitude - event_city.longitude) / 2), 2)
                        )))
                    )::integer
                ) AS distance_km,
                preferences.radius_km
            FROM fan_location_preferences preferences
            INNER JOIN fans
                ON fans.workspace_id = preferences.workspace_id
               AND fans.id = preferences.fan_id
            INNER JOIN cities fan_city ON fan_city.id = preferences.city_id
            INNER JOIN events
                ON events.workspace_id = preferences.workspace_id
               AND events.status = 'published'
               AND events.starts_at > now()
               AND events.starts_at < now() + interval '365 days'
            INNER JOIN cities event_city ON event_city.id = events.city_id
            WHERE preferences.workspace_id = $1
              AND preferences.nearby_gigs_enabled
              AND fans.status = 'active'
              AND fan_city.latitude IS NOT NULL
              AND fan_city.longitude IS NOT NULL
              AND event_city.latitude IS NOT NULL
              AND event_city.longitude IS NOT NULL
        ),
        inserted AS (
            INSERT INTO nearby_gig_notifications (
                workspace_id, fan_id, event_id, distance_km
            )
            SELECT $1, fan_id, event_id, distance_km
            FROM candidates
            WHERE distance_km <= radius_km
            ON CONFLICT DO NOTHING
            RETURNING fan_id, event_id, distance_km
        ),
        queued AS (
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id
            )
            SELECT
                $1,
                'fan.nearby_concert_available',
                1,
                jsonb_build_object(
                    'fan_id', candidates.fan_id,
                    'email', candidates.normalized_email,
                    'event_id', candidates.event_id,
                    'event_slug', candidates.event_slug,
                    'event_title', candidates.event_title,
                    'starts_at', candidates.starts_at,
                    'venue', candidates.venue,
                    'city_name', candidates.event_city,
                    'distance_km', inserted.distance_km
                ),
                $2
            FROM inserted
            INNER JOIN candidates
                ON candidates.fan_id = inserted.fan_id
               AND candidates.event_id = inserted.event_id
            RETURNING 1
        )
        SELECT count(*) FROM queued
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(request_id_value.as_deref())
    .fetch_one(state.ticketing.pool())
    .await;
    match queued {
        Ok(count) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(json!({"queued": count})),
        )
            .into_response(),
        Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
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
