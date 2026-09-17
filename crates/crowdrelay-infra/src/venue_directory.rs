//! The resolution anchors and identity writes of the external data layer
//! (§12-3, §12-4).
//!
//! `place_venues` keyed on `(city_id, name_key)` is enough when the only
//! writers are tenants typing a room's name and the researched sheet. The
//! moment a directory answers "the room at these coordinates" or "the room
//! this OSM node is", a name is the weakest key on the table — the same room
//! arrives as `Klub Stodoła`, `STODOLA` and OSM node 302516611, and an
//! unresolved duplicate splits the evidence the room earned.
//!
//! `place_venue_identifiers` is the anchor that stops that: `(scheme,
//! identifier)` is the primary key, so one OSM node, one Wikidata Qid or one
//! MusicBrainz id points at exactly one room whoever saw it first. Resolution
//! asks the anchor *before* it mints — a known identifier is the cheapest and
//! most certain hit in the §12-4 order.
//!
//! The writes here are deliberately idempotent upserts run in anchor-first
//! order (resolve → mint → link → facts, the facts via
//! `PostgresVenueSeedRepository::write_fact`). Every statement is safe to
//! replay, so a sweep interrupted mid-element simply finishes the link on its
//! next pass — there is no half-written state a retry cannot complete.

use sqlx::PgPool;
use uuid::Uuid;

/// The identity side of the venue registry: anchors and the minted room row.
/// Facts about the room stay with `PostgresVenueSeedRepository::write_fact` —
/// this repository only answers *which* room a source is talking about.
#[derive(Clone)]
pub struct PostgresVenueDirectoryRepository {
    pool: PgPool,
}

impl PostgresVenueDirectoryRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Where one `(scheme, identifier)` anchor points, when it is known.
    /// `Ok(None)` is "this source has never named a room we hold" — the
    /// caller's cue to mint.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn resolve_identifier(
        &self,
        scheme: &str,
        identifier: &str,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT venue_id
            FROM place_venue_identifiers
            WHERE scheme = $1 AND identifier = $2
            "#,
        )
        .bind(scheme)
        .bind(identifier)
        .fetch_optional(&self.pool)
        .await
    }

    /// Anchor one external identifier to a room.
    ///
    /// Returns the venue the anchor actually points at — `venue_id` on a
    /// fresh link, and the already-anchored room when the identifier was
    /// bound earlier. The primary key is the whole resolution rule: one
    /// external id is one room, so the existing binding wins over the newest
    /// claimant rather than flipping or failing. A caller that minted a row
    /// and then links it must use the returned id, not the minted one —
    /// the anchor may already belong to the same room under an older name.
    ///
    /// # Errors
    ///
    /// Propagates the database error — including the CHECK violation an
    /// empty or over-long identifier earns.
    pub async fn link_venue_identifier(
        &self,
        venue_id: Uuid,
        scheme: &str,
        identifier: &str,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO place_venue_identifiers (scheme, identifier, venue_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (scheme, identifier) DO NOTHING
            "#,
        )
        .bind(scheme)
        .bind(identifier)
        .bind(venue_id)
        .execute(&self.pool)
        .await?;
        // The anchor read back, not the id asked for: a conflict means the
        // identifier was already somebody's room, and that binding is the
        // answer resolution exists to give.
        let bound = self.resolve_identifier(scheme, identifier).await?;
        Ok(bound.unwrap_or(venue_id))
    }

    /// Mint or refresh the identity row for a room a directory saw.
    ///
    /// `ON CONFLICT (city_id, name_key) DO UPDATE` refreshes the coordinates
    /// only — a re-sweep may move the pin, but it may not rewrite what the
    /// room is called for the tenants who already play it. A name with no
    /// normalisable content mints nothing: `place_venue_key` answers NULL and
    /// the insert fails its own CHECK rather than filing a keyless room, so
    /// callers skip nameless elements before asking.
    ///
    /// # Errors
    ///
    /// Propagates the database error.
    pub async fn upsert_venue(
        &self,
        city_id: Uuid,
        name: &str,
        latitude: Option<f64>,
        longitude: Option<f64>,
    ) -> Result<Uuid, sqlx::Error> {
        // The CHECK bounds display_name and name_key at 500; bounding the
        // name once, before the key is derived, keeps a directory's abuse of
        // the tag a skipped row rather than a failed statement.
        let name: String = name.trim().chars().take(500).collect();
        sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO place_venues (city_id, name_key, display_name, latitude, longitude)
            VALUES ($1, place_venue_key($2), $3, $4, $5)
            ON CONFLICT (city_id, name_key)
            DO UPDATE SET latitude = COALESCE(EXCLUDED.latitude, place_venues.latitude),
                          longitude = COALESCE(EXCLUDED.longitude, place_venues.longitude)
            RETURNING id
            "#,
        )
        .bind(city_id)
        .bind(&name)
        .bind(&name)
        .bind(latitude)
        .bind(longitude)
        .fetch_one(&self.pool)
        .await
    }
}
