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

/// A city fans asked for that still has no coordinates, with the demand behind
/// it. Reaching a fan by proximity needs coordinates on both ends, so until
/// somebody supplies them every fan sitting in one of these is unreachable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, FromRow)]
pub struct PendingCity {
    pub city_id: Uuid,
    pub slug: String,
    pub name: String,
    pub country_code: String,
    pub region: Option<String>,
    pub request_count: i32,
    pub waiting_fans: i64,
}

/// What a fan's location targeting actually looks like after a change.
///
/// `targeting_ready` is the honest answer to "will nearby shows reach me": it
/// is false while the chosen city has no coordinates, which happens for a city
/// a fan requested and nobody has geocoded yet. Without it the client can only
/// report that the preference was saved, which is true and useless -- the fan
/// hears nothing and has no way to know why.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FanLocationState {
    pub city_slug: String,
    pub city_name: String,
    pub nearby_gigs_enabled: bool,
    pub radius_km: i32,
    pub targeting_ready: bool,
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
    lease_expired: bool,
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
    /// for. Returns whether a preference row was actually written.
    ///
    /// City slugs are only unique per country (`cities UNIQUE (country_code,
    /// slug)`), so the slug alone cannot identify a city. The interest row the
    /// signup transaction wrote was resolved against the configured country,
    /// so joining it keeps this write on the same city instead of matching a
    /// same-slug city in another country.
    ///
    /// The join can match nothing, and that is not hypothetical: a repeat
    /// signup for an address that is already pending or active returns early
    /// without writing a city interest, so a fan naming a different city the
    /// second time has no interest row for it. This used to insert zero rows
    /// and return `Ok(())`, the caller only logged on `Err`, and the app showed
    /// the toggle as saved -- so the fan opted into nearby shows, was told it
    /// worked, and never heard anything again. The caller can see the miss now.
    pub async fn upsert_fan_location_preference(
        &self,
        fan_id: FanId,
        city_slug: &str,
        nearby_gigs_enabled: bool,
        radius_km: i32,
    ) -> Result<bool, MobileFanStoreError> {
        self.bounded(async {
            let written = sqlx::query(
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
            Ok(written.rows_affected() == 1)
        })
        .await
    }

    /// Sets the city and nearby-show preference for a fan who has proved a
    /// session, and reports whether targeting can actually work.
    ///
    /// The only other writer of a location preference is the signup handler,
    /// and it refuses to touch an address that is already pending or active --
    /// correctly, because that submission is unauthenticated and an
    /// unconfirmed address is not proof of anything. The gap that leaves is
    /// wide: a fan who bought a ticket has an `active` row and never signed
    /// up, and a fan who moved cannot change city. Neither could establish a
    /// location at all, and the nearby loop is keyed entirely on one.
    ///
    /// A proved session closes that without reopening the poisoning hole, so
    /// this is the authenticated counterpart rather than a relaxation of the
    /// signup rule. It writes only tenant-scoped rows; `cities` is a shared
    /// catalogue and is read here, never written.
    ///
    /// Idempotent by construction: the interest insert is `DO NOTHING` and the
    /// preference is an upsert, so a replay lands on the same state and the
    /// city aggregate counts a fan once.
    pub async fn set_fan_location(
        &self,
        fan_id: FanId,
        city_slug: &str,
        country_code: &str,
        nearby_gigs_enabled: bool,
        radius_km: i32,
    ) -> Result<Option<FanLocationState>, MobileFanStoreError> {
        self.bounded(async {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|_| MobileFanStoreError::Unavailable)?;

            // Slugs are unique per country, never globally, so the country has
            // to be part of the lookup or a same-slug city elsewhere matches.
            let Some((city_id, city_name, has_coordinates)) =
                sqlx::query_as::<_, (Uuid, String, bool)>(
                    r#"
                    SELECT
                        id,
                        name,
                        latitude IS NOT NULL AND longitude IS NOT NULL
                    FROM cities
                    WHERE country_code = $1 AND slug = $2
                    "#,
                )
                .bind(country_code)
                .bind(city_slug)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|_| MobileFanStoreError::Unavailable)?
            else {
                return Ok(None);
            };

            let interest_created = sqlx::query_scalar::<_, i32>(
                r#"
                INSERT INTO fan_city_interests (workspace_id, fan_id, city_id)
                VALUES ($1, $2, $3)
                ON CONFLICT (workspace_id, fan_id, city_id) DO NOTHING
                RETURNING 1
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .bind(city_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?
            .is_some();

            // Mirrors the signup path: an active fan joining a city they were
            // not already interested in adds one to that city's confirmed
            // count. A fan who is not active is counted when they confirm, so
            // counting here too would count them twice.
            if interest_created {
                sqlx::query(
                    r#"
                    INSERT INTO city_aggregates (workspace_id, city_id, confirmed_fan_count)
                    SELECT $1, $2, 1
                    WHERE EXISTS (
                        SELECT 1 FROM fans
                        WHERE workspace_id = $1 AND id = $3 AND status = 'active'
                    )
                    ON CONFLICT (workspace_id, city_id) DO UPDATE
                    SET confirmed_fan_count = city_aggregates.confirmed_fan_count + 1,
                        updated_at = now()
                    "#,
                )
                .bind(self.workspace_id.into_uuid())
                .bind(city_id)
                .bind(fan_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(|_| MobileFanStoreError::Unavailable)?;
            }

            sqlx::query(
                r#"
                INSERT INTO fan_location_preferences (
                    workspace_id, fan_id, city_id, nearby_gigs_enabled, radius_km
                )
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (workspace_id, fan_id) DO UPDATE
                SET city_id = EXCLUDED.city_id,
                    nearby_gigs_enabled = EXCLUDED.nearby_gigs_enabled,
                    radius_km = EXCLUDED.radius_km
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .bind(city_id)
            .bind(nearby_gigs_enabled)
            .bind(radius_km)
            .execute(&mut *transaction)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?;

            transaction
                .commit()
                .await
                .map_err(|_| MobileFanStoreError::Unavailable)?;

            Ok(Some(FanLocationState {
                city_slug: city_slug.to_owned(),
                city_name,
                nearby_gigs_enabled,
                radius_km,
                targeting_ready: has_coordinates,
            }))
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
        approve: bool,
    ) -> Result<bool, MobileFanStoreError> {
        self.bounded(async {
            let updated = sqlx::query(
                r#"
                UPDATE cities
                SET latitude = $2,
                    longitude = $3,
                    name = COALESCE($4, name),
                    region = COALESCE($5, region),
                    moderation_status = CASE
                        WHEN $6 THEN 'approved'
                        ELSE moderation_status
                    END
                WHERE id = $1
                  AND moderation_status IN ('pending', 'approved')
                "#,
            )
            .bind(city_id)
            .bind(latitude)
            .bind(longitude)
            .bind(canonical_name)
            .bind(region)
            .bind(approve)
            .execute(&self.pool)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?;
            Ok(updated.rows_affected() == 1)
        })
        .await
    }

    /// Lists the cities fans requested that still cannot reach anybody, most
    /// wanted first.
    ///
    /// `request_city` records a name, never coordinates, and the nearby-gig
    /// query needs them on both ends. Signup resolves a city without checking
    /// moderation status, so fans keep arriving into these and waiting. Nothing
    /// listed them and nothing called the geocode endpoint beside this one, so
    /// the queue was invisible: the ops snapshot counted them and stopped
    /// there. `waiting_fans` is the cost of leaving each one alone.
    pub async fn list_pending_cities(
        &self,
        limit: i64,
    ) -> Result<Vec<PendingCity>, MobileFanStoreError> {
        self.bounded(async {
            sqlx::query_as::<_, PendingCity>(
                r#"
                SELECT
                    cities.id AS city_id,
                    cities.slug,
                    cities.name,
                    cities.country_code::text AS country_code,
                    cities.region,
                    cities.request_count,
                    count(preference.fan_id) FILTER (
                        WHERE preference.nearby_gigs_enabled
                          AND fan.status = 'active'
                    ) AS waiting_fans
                FROM cities
                LEFT JOIN fan_location_preferences AS preference
                    ON preference.city_id = cities.id
                   AND preference.workspace_id = $1
                LEFT JOIN fans AS fan
                    ON fan.workspace_id = preference.workspace_id
                   AND fan.id = preference.fan_id
                WHERE cities.moderation_status = 'pending'
                   OR cities.latitude IS NULL
                   OR cities.longitude IS NULL
                GROUP BY cities.id
                ORDER BY waiting_fans DESC, cities.request_count DESC, cities.name, cities.id
                LIMIT $2
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(limit.clamp(1, 200))
            .fetch_all(&self.pool)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)
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
                      -- Current marketing consent gates both channels. It used
                      -- to gate only push, and `fans.status = 'active'` was
                      -- doing the work for mail on the reasoning that
                      -- unsubscribing moves the status. That covers a fan who
                      -- consented and then withdrew; it does not cover a fan
                      -- who never consented at all, and those exist: a ticket
                      -- purchase creates an `active` fan row with no consent
                      -- row, correctly, because buying is not subscribing.
                      -- Such a fan could not reach this query while nothing
                      -- could give them a location preference. Now that they
                      -- can set one, the missing check would be the difference
                      -- between lawful and not, so it moves here where both
                      -- channels read it rather than staying on one of them.
                      AND EXISTS (
                          SELECT 1
                          FROM fan_consents AS consent
                          WHERE consent.workspace_id = preferences.workspace_id
                            AND consent.fan_id = preferences.fan_id
                            AND consent.purpose = 'marketing'
                            AND consent.granted
                            AND consent.id = (
                                SELECT newest.id
                                FROM fan_consents AS newest
                                WHERE newest.workspace_id = consent.workspace_id
                                  AND newest.fan_id = consent.fan_id
                                  AND newest.purpose = consent.purpose
                                ORDER BY newest.recorded_at DESC, newest.id DESC
                                LIMIT 1
                            )
                      )
                      AND fan_city.latitude IS NOT NULL
                      AND fan_city.longitude IS NOT NULL
                      AND event_city.latitude IS NOT NULL
                      AND event_city.longitude IS NOT NULL
                      -- Latitude is a cheap, exact lower bound on great-circle
                      -- distance: one degree of latitude is 111.19 km wherever
                      -- you stand, so a pair further apart than the radius in
                      -- latitude alone can never be inside it. Dividing by
                      -- 111.0 rather than 111.19, and allowing one extra
                      -- kilometre, keeps the box strictly wider than the test
                      -- below -- including for the pair that only passes it
                      -- because `distance_km` is rounded down to the radius.
                      -- No safe equivalent exists for longitude: a degree of it
                      -- is worth less distance the further north you are, so
                      -- the same bound would drop real matches.
                      AND abs(fan_city.latitude - event_city.latitude)
                          <= (preferences.radius_km + 1)::double precision / 111.0
                      -- Every run recomputed the haversine for every fan-event
                      -- pair ever notified and threw the result away in the
                      -- INSERT's ON CONFLICT. The work grew with the whole
                      -- fanbase times the whole calendar while the useful
                      -- output stayed proportional to what is new, and the
                      -- statement runs under a five-second operation timeout --
                      -- so the notification loop was on course to stop
                      -- delivering exactly when the fanbase got big enough to
                      -- matter. The insert still guards the race; this only
                      -- stops the pointless arithmetic reaching it.
                      AND NOT EXISTS (
                          SELECT 1
                          FROM nearby_gig_notifications sent
                          WHERE sent.workspace_id = preferences.workspace_id
                            AND sent.fan_id = preferences.fan_id
                            AND sent.event_id = events.id
                      )
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
                    -- Consent is checked once in `candidates` now, for both
                    -- channels, so this only asks whether push is switched on.
                    WHERE $3::boolean
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
            SELECT
                request_hash,
                state,
                response_body,
                COALESCE(lease_expires_at <= now(), false) AS lease_expired
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
        // A lease was written on the way in and then never read, so a request
        // that died between claiming the key and completing it left the row
        // `in_progress` until its 24-hour retention expired. Every retry of
        // that key answered 503 for the rest of the day, and the city picker is
        // the first screen of onboarding -- the one place a fan cannot route
        // around. Reclaim an expired lease the way the signup path already
        // does, and keep 503 for the case it was actually describing: another
        // request holding a live lease right now.
        if inserted.rows_affected() != 1 {
            if !idempotency.lease_expired {
                return Err(MobileFanStoreError::Conflict);
            }
            let reclaimed = sqlx::query(
                r#"
                UPDATE idempotency_keys
                SET lease_owner = $4,
                    lease_expires_at =
                        now() + ($5::bigint * interval '1 second')
                WHERE workspace_id = $1
                  AND scope = $2
                  AND key = $3
                  AND state = 'in_progress'
                  AND lease_expires_at <= now()
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(CITY_REQUEST_SCOPE)
            .bind(command.idempotency_key.as_str())
            .bind(&lease_owner)
            .bind(CITY_REQUEST_LEASE_SECONDS)
            .execute(&mut *transaction)
            .await
            .map_err(|_| MobileFanStoreError::Unavailable)?;
            if reclaimed.rows_affected() != 1 {
                return Err(MobileFanStoreError::Conflict);
            }
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
