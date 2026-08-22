//! PostgreSQL persistence for mobile-fan onboarding and nearby-gig delivery.
//!
//! HTTP handlers validate/normalize transport input. This adapter owns durable
//! writes, transactional city-request idempotency and nearby notification SQL.

use std::{future::Future, time::Duration};

use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::{FanId, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;
use tokio::time::timeout;
use uuid::Uuid;

const CITY_REQUEST_SCOPE: &str = "city_request";
const CITY_REQUEST_RETENTION_SECONDS: i64 = 24 * 60 * 60;
const CITY_REQUEST_LEASE_SECONDS: i64 = 30;
const CITY_NOTIFICATION_COOLDOWN_MINUTES: i64 = 60;
const JSON_CONTENT_TYPE: &str = "application/json";

#[derive(Clone, Debug)]
pub struct PostgresMobileFanRepository {
    pool: PgPool,
    workspace_id: WorkspaceId,
    operation_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct CityRequestCommand {
    pub idempotency_key: IdempotencyKey,
    pub request_id: Option<String>,
    pub name: String,
    pub region: Option<String>,
    pub country_code: String,
    pub slug: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CityRequestResult {
    pub city_slug: String,
    pub display_name: String,
    pub status: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum MobileFanStoreError {
    #[error("request conflicts with an existing idempotency key")]
    Conflict,
    #[error("mobile fan persistence is temporarily unavailable")]
    Unavailable,
}

#[derive(FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    state: String,
    response_body: Option<Value>,
}

impl PostgresMobileFanRepository {
    #[must_use]
    pub fn new(pool: PgPool, workspace_id: WorkspaceId, operation_timeout: Duration) -> Self {
        Self {
            pool,
            workspace_id,
            operation_timeout,
        }
    }

    async fn bounded<T>(
        &self,
        future: impl Future<Output = Result<T, MobileFanStoreError>>,
    ) -> Result<T, MobileFanStoreError> {
        timeout(self.operation_timeout, future)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?
    }

    pub async fn request_city(
        &self,
        command: &CityRequestCommand,
    ) -> Result<CityRequestResult, MobileFanStoreError> {
        self.bounded(self.request_city_inner(command)).await
    }

    /// Records the fan's nearby-gig preference against the city they signed up
    /// for.
    ///
    /// City slugs are only unique per country (`cities UNIQUE (country_code,
    /// slug)`), so the slug alone cannot identify a city. The interest row the
    /// signup transaction wrote was resolved against the configured country,
    /// so joining it keeps this write on the same city instead of matching a
    /// same-slug city in another country.
    pub async fn upsert_fan_location_preference(
        &self,
        fan_id: FanId,
        city_slug: &str,
        nearby_gigs_enabled: bool,
        radius_km: i32,
    ) -> Result<(), MobileFanStoreError> {
        self.bounded(async {
            sqlx::query(
                r#"
                INSERT INTO fan_location_preferences (
                    workspace_id, fan_id, city_id, nearby_gigs_enabled, radius_km
                )
                SELECT $1, $2, cities.id, $4, $5
                FROM cities
                INNER JOIN fan_city_interests AS interest
                    ON interest.city_id = cities.id
                    AND interest.workspace_id = $1
                    AND interest.fan_id = $2
                WHERE cities.slug = $3
                ON CONFLICT (workspace_id, fan_id) DO UPDATE
                SET city_id = EXCLUDED.city_id,
                    nearby_gigs_enabled = EXCLUDED.nearby_gigs_enabled,
                    radius_km = EXCLUDED.radius_km
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .bind(city_slug)
            .bind(nearby_gigs_enabled)
            .bind(radius_km)
            .execute(&self.pool)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?;
            Ok(())
        })
        .await
    }

    pub async fn geocode_city(
        &self,
        city_id: Uuid,
        latitude: f64,
        longitude: f64,
        canonical_name: Option<String>,
        region: Option<String>,
    ) -> Result<bool, MobileFanStoreError> {
        self.bounded(async {
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
            .bind(latitude)
            .bind(longitude)
            .bind(canonical_name)
            .bind(region)
            .execute(&self.pool)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?;
            Ok(updated.rows_affected() == 1)
        })
        .await
    }

    pub async fn emit_due_nearby_gigs(
        &self,
        request_id: Option<&str>,
        push_enabled: bool,
    ) -> Result<(i64, i64), MobileFanStoreError> {
        self.bounded(async {
            sqlx::query_as::<_, (i64, i64)>(
                r#"
                WITH candidates AS (
                    SELECT
                        fans.id AS fan_id,
                        fans.normalized_email,
                        fans.locale,
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
                ),
                push_queued AS (
                    INSERT INTO fan_push_deliveries (
                        workspace_id, fan_id, endpoint_id, source_kind, source_id, category,
                        title, body, target_path, collapse_key
                    )
                    SELECT
                        $1,
                        candidates.fan_id,
                        endpoint.id,
                        'nearby_concert',
                        candidates.event_id,
                        'shows',
                        CASE WHEN lower(COALESCE(candidates.locale, 'pl')) LIKE 'pl%'
                            THEN 'VIRYA blisko Ciebie'
                            ELSE 'VIRYA near you'
                        END,
                        CASE WHEN lower(COALESCE(candidates.locale, 'pl')) LIKE 'pl%'
                            THEN candidates.event_title || ' — koncert około ' || inserted.distance_km || ' km od Twojego miasta.'
                            ELSE candidates.event_title || ' — a show about ' || inserted.distance_km || ' km from your city.'
                        END,
                        CASE WHEN lower(COALESCE(candidates.locale, 'pl')) LIKE 'pl%'
                            THEN '/pl/my-signal/?event=' || candidates.event_slug
                            ELSE '/my-signal/?event=' || candidates.event_slug
                        END,
                        'nearby:' || candidates.event_id::text
                    FROM inserted
                    JOIN candidates
                      ON candidates.fan_id = inserted.fan_id
                     AND candidates.event_id = inserted.event_id
                    JOIN fan_push_endpoints endpoint
                      ON endpoint.workspace_id = $1
                     AND endpoint.fan_id = candidates.fan_id
                     AND endpoint.active
                     AND endpoint.invalidated_at IS NULL
                    WHERE $3::boolean
                      AND EXISTS (
                          SELECT 1
                          FROM fan_consents consent
                          WHERE consent.workspace_id = $1
                            AND consent.fan_id = candidates.fan_id
                            AND consent.purpose = 'marketing'
                            AND consent.granted
                            AND consent.id = (
                                SELECT newest.id
                                FROM fan_consents newest
                                WHERE newest.workspace_id = consent.workspace_id
                                  AND newest.fan_id = consent.fan_id
                                  AND newest.purpose = consent.purpose
                                ORDER BY newest.recorded_at DESC, newest.id DESC
                                LIMIT 1
                            )
                      )
                    ON CONFLICT (workspace_id, source_kind, source_id, endpoint_id) DO NOTHING
                    RETURNING 1
                )
                SELECT
                    (SELECT count(*)::bigint FROM queued),
                    (SELECT count(*)::bigint FROM push_queued)
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(request_id)
            .bind(push_enabled)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)
        })
        .await
    }

    async fn request_city_inner(
        &self,
        command: &CityRequestCommand,
    ) -> Result<CityRequestResult, MobileFanStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?;
        let request_hash = city_request_hash(command)?;
        let lease_owner = command
            .request_id
            .clone()
            .unwrap_or_else(|| format!("city-request-{}", Uuid::now_v7().simple()));

        sqlx::query(
            r#"
            DELETE FROM idempotency_keys
            WHERE workspace_id = $1
              AND scope = $2
              AND key = $3
              AND expires_at <= now()
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(CITY_REQUEST_SCOPE)
        .bind(command.idempotency_key.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|_| MobileFanStoreError::Unavailable)?;

        let inserted = sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id, scope, key, request_hash, state,
                lease_owner, lease_expires_at, expires_at
            )
            VALUES (
                $1, $2, $3, $4, 'in_progress', $5,
                now() + ($6::bigint * interval '1 second'),
                now() + ($7::bigint * interval '1 second')
            )
            ON CONFLICT (workspace_id, scope, key) DO NOTHING
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(CITY_REQUEST_SCOPE)
        .bind(command.idempotency_key.as_str())
        .bind(request_hash.as_slice())
        .bind(&lease_owner)
        .bind(CITY_REQUEST_LEASE_SECONDS)
        .bind(CITY_REQUEST_RETENTION_SECONDS)
        .execute(&mut *transaction)
        .await
        .map_err(|_| MobileFanStoreError::Unavailable)?;

        let idempotency = sqlx::query_as::<_, IdempotencyRow>(
            r#"
            SELECT request_hash, state, response_body
            FROM idempotency_keys
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
            FOR UPDATE
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(CITY_REQUEST_SCOPE)
        .bind(command.idempotency_key.as_str())
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| MobileFanStoreError::Unavailable)?;

        if idempotency.request_hash != request_hash.as_slice() {
            return Err(MobileFanStoreError::Conflict);
        }
        if idempotency.state == "completed" {
            let response = idempotency
                .response_body
                .ok_or(MobileFanStoreError::Unavailable)
                .and_then(|value| {
                    serde_json::from_value(value).map_err(|_| MobileFanStoreError::Unavailable)
                })?;
            transaction
                .commit()
                .await
                .map_err(|_| MobileFanStoreError::Unavailable)?;
            return Ok(response);
        }
        if inserted.rows_affected() != 1 {
            return Err(MobileFanStoreError::Unavailable);
        }

        let result = match find_approved_city(&mut transaction, command).await? {
            Some((city_slug, display_name)) => CityRequestResult {
                city_slug,
                display_name,
                status: "approved".to_owned(),
            },
            None => upsert_pending_city(&mut transaction, self.workspace_id, command).await?,
        };
        complete_city_request_idempotency(
            &mut transaction,
            self.workspace_id,
            command,
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?;
        Ok(result)
    }
}

fn city_request_hash(command: &CityRequestCommand) -> Result<[u8; 32], MobileFanStoreError> {
    let canonical = serde_json::to_vec(&(
        command.name.as_str(),
        command.region.as_deref(),
        command.country_code.as_str(),
        command.slug.as_str(),
    ))
    .map_err(|_| MobileFanStoreError::Unavailable)?;
    Ok(Sha256::digest(canonical).into())
}

async fn find_approved_city(
    transaction: &mut Transaction<'_, Postgres>,
    command: &CityRequestCommand,
) -> Result<Option<(String, String)>, MobileFanStoreError> {
    sqlx::query_as::<_, (String, String)>(
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
    .bind(&command.country_code)
    .bind(&command.name)
    .bind(command.region.as_deref())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| MobileFanStoreError::Unavailable)
}

async fn upsert_pending_city(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &CityRequestCommand,
) -> Result<CityRequestResult, MobileFanStoreError> {
    let (city_id, city_slug, display_name, request_count) =
        sqlx::query_as::<_, (Uuid, String, String, i32)>(
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
        .bind(&command.slug)
        .bind(&command.name)
        .bind(&command.country_code)
        .bind(command.region.as_deref())
        .fetch_one(&mut **transaction)
        .await
        .map_err(|_| MobileFanStoreError::Unavailable)?;

    sqlx::query(
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
    .bind(workspace_id.into_uuid())
    .bind(city_id)
    .bind(&city_slug)
    .bind(&display_name)
    .bind(&command.country_code)
    .bind(request_count)
    .bind(command.request_id.as_deref())
    .bind(CITY_NOTIFICATION_COOLDOWN_MINUTES)
    .execute(&mut **transaction)
    .await
    .map_err(|_| MobileFanStoreError::Unavailable)?;

    Ok(CityRequestResult {
        city_slug,
        display_name,
        status: "pending".to_owned(),
    })
}

async fn complete_city_request_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &CityRequestCommand,
    request_hash: &[u8; 32],
    result: &CityRequestResult,
) -> Result<(), MobileFanStoreError> {
    let response_status = if result.status == "approved" {
        200
    } else {
        202
    };
    let response_body =
        serde_json::to_value(result).map_err(|_| MobileFanStoreError::Unavailable)?;
    let updated = sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET state = 'completed',
            lease_owner = NULL,
            lease_expires_at = NULL,
            response_status = $5,
            response_body = $6,
            response_content_type = $7,
            completed_at = now()
        WHERE workspace_id = $1
          AND scope = $2
          AND key = $3
          AND request_hash = $4
          AND state = 'in_progress'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(CITY_REQUEST_SCOPE)
    .bind(command.idempotency_key.as_str())
    .bind(request_hash.as_slice())
    .bind(response_status)
    .bind(response_body)
    .bind(JSON_CONTENT_TYPE)
    .execute(&mut **transaction)
    .await
    .map_err(|_| MobileFanStoreError::Unavailable)?;
    if updated.rows_affected() != 1 {
        return Err(MobileFanStoreError::Conflict);
    }
    Ok(())
}
