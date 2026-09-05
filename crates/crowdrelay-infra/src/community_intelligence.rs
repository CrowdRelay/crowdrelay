//! Community Intelligence persistence.
//!
//! All SQL for the community observation layer lives here so the HTTP layer
//! stays free of write statements (see the api-sql ratchet) and the worker
//! sweep can share the exact same statements.
//!
//! Every repository method takes `workspace_id` as a parameter and includes
//! `WHERE workspace_id = $1` in its SQL. Tenant isolation is enforced at the
//! query level, not just at the handler level. A handler passing a wrong ID
//! cannot leak another tenant's observations.

use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crowdrelay_domain::community_intelligence::{CommunityEntity, CommunityObservation};

#[derive(Clone)]
pub struct PostgresCommunityIntelligenceRepository {
    pool: PgPool,
}

#[derive(Debug, thiserror::Error)]
pub enum CommunityIntelligenceError {
    #[error("community intelligence database operation failed")]
    Database(sqlx::Error),
}

impl CommunityIntelligenceError {
    fn unexpected(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "community intelligence persistence failed");
        CommunityIntelligenceError::Database(error)
    }
}

impl From<sqlx::Error> for CommunityIntelligenceError {
    fn from(error: sqlx::Error) -> Self {
        Self::unexpected(error)
    }
}

/// A persisted observation row (read model).
#[derive(Debug, FromRow)]
pub struct ObservationRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub place_id: Uuid,
    pub observed_at: OffsetDateTime,
    pub source: String,
    pub source_url: String,
    pub collector_version: String,
    pub raw_activity_metrics: serde_json::Value,
    pub observation_quality: i32,
    pub created_at: OffsetDateTime,
}

/// A persisted entity row (read model).
#[derive(Debug, FromRow)]
pub struct EntityRow {
    pub id: Uuid,
    pub observation_id: Uuid,
    pub entity_type: String,
    pub entity_ref: String,
    pub strength: i32,
    pub observed_at: OffsetDateTime,
}

/// A place row with its latest observation (for list views).
#[derive(Debug, FromRow)]
pub struct PlaceWithLatestObservation {
    pub place_id: Uuid,
    pub place_kind: String,
    pub platform: String,
    pub name: String,
    pub url: String,
    pub country_code: Option<String>,
    pub language: Option<String>,
    pub genres: Vec<String>,
    pub member_count: Option<i32>,
    /// Our relationship with the place, not whether we observe it.
    ///
    /// The console listed 66 tracked communities with no way to record
    /// "joined", so every visit started from zero and joining never became a
    /// task anybody picked up.
    pub membership_state: String,
    pub membership_note: Option<String>,
    pub membership_changed_at: Option<OffsetDateTime>,
    pub latest_observation_id: Option<Uuid>,
    pub latest_observed_at: Option<OffsetDateTime>,
    pub latest_source: Option<String>,
    pub latest_observation_quality: Option<i32>,
    pub latest_raw_activity_metrics: Option<serde_json::Value>,
}

impl PostgresCommunityIntelligenceRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Atomically inserts an observation and its extracted entities.
    /// The entire insert is wrapped in a transaction — either all rows
    /// land or none do.
    pub async fn insert_observation(
        &self,
        workspace_id: Uuid,
        place_id: Uuid,
        observation: &CommunityObservation,
        entities: &[CommunityEntity],
    ) -> Result<ObservationRow, CommunityIntelligenceError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(CommunityIntelligenceError::unexpected)?;

        let row = sqlx::query_as::<_, ObservationRow>(
            r#"INSERT INTO community_observations
                 (workspace_id, place_id, source, source_url, collector_version,
                  raw_activity_metrics, observation_quality)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               RETURNING id, workspace_id, place_id, observed_at, source,
                         source_url, collector_version, raw_activity_metrics,
                         observation_quality, created_at"#,
        )
        .bind(workspace_id)
        .bind(place_id)
        .bind(&observation.source)
        .bind(&observation.source_url)
        .bind(&observation.collector_version)
        .bind(&observation.raw_activity_metrics)
        .bind(observation.observation_quality)
        .fetch_one(&mut *tx)
        .await?;

        // One statement, not one per entity.
        //
        // An observation carries every genre its source mentioned — around
        // twenty for a busy subreddit — and the Reddit adapter sweeps 28
        // places, so a per-entity insert is roughly 560 round trips inside a
        // single transaction that holds a lock the whole time.
        if !entities.is_empty() {
            let types: Vec<&str> = entities
                .iter()
                .map(|entity| entity.entity_type.as_db_str())
                .collect();
            let refs: Vec<&str> = entities
                .iter()
                .map(|entity| entity.entity_ref.as_str())
                .collect();
            let strengths: Vec<i32> = entities.iter().map(|entity| entity.strength).collect();
            sqlx::query(
                r#"INSERT INTO community_entities
                     (observation_id, entity_type, entity_ref, strength)
                   SELECT $1, entity_type, entity_ref, strength
                   FROM UNNEST($2::text[], $3::text[], $4::int[])
                     AS batch(entity_type, entity_ref, strength)"#,
            )
            .bind(row.id)
            .bind(&types)
            .bind(&refs)
            .bind(&strengths)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit()
            .await
            .map_err(CommunityIntelligenceError::unexpected)?;
        Ok(row)
    }

    /// Returns the most recent observation per place for a workspace.
    /// Uses DISTINCT ON to get one row per place_id, ordered by observed_at DESC.
    /// How long ago each source last produced an observation.
    ///
    /// The sweep used to schedule the first run of every source a full
    /// interval after process start. Reddit is observed every 12 hours, so a
    /// worker restarted more often than that — which is to say, on any day
    /// with a deploy — would never reach its first sweep at all. The clock
    /// belongs to the data, not to the process.
    ///
    /// A source absent from the result has never observed anything and is due
    /// immediately.
    ///
    /// Scoped to one workspace, like every other read in this file. It was not:
    /// the aggregate ran over the whole table, so in a database holding two
    /// workspaces one tenant's observations would satisfy the other's freshness
    /// check and postpone its sweep by a full interval. The tenant that never
    /// discovered a community would have had nothing in the log to say why.
    /// Today one deployment serves one workspace, so this could not yet
    /// mis-fire, which is exactly why it was worth fixing before it could.
    pub async fn seconds_since_last_observation(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<(String, f64)>, CommunityIntelligenceError> {
        sqlx::query_as::<_, (String, f64)>(
            r#"SELECT source,
                      EXTRACT(EPOCH FROM (now() - max(observed_at)))::double precision
               FROM community_observations
               WHERE workspace_id = $1
               GROUP BY source"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(CommunityIntelligenceError::unexpected)
    }

    pub async fn latest_observations(
        &self,
        workspace_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ObservationRow>, CommunityIntelligenceError> {
        sqlx::query_as::<_, ObservationRow>(
            r#"SELECT id, workspace_id, place_id, observed_at, source,
                      source_url, collector_version, raw_activity_metrics,
                      observation_quality, created_at
               FROM community_observations
               WHERE workspace_id = $1
               ORDER BY observed_at DESC
               LIMIT $2"#,
        )
        .bind(workspace_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(CommunityIntelligenceError::unexpected)
    }

    /// Returns the observation time series for one place (tenant-scoped).
    pub async fn observations_for_place(
        &self,
        workspace_id: Uuid,
        place_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ObservationRow>, CommunityIntelligenceError> {
        sqlx::query_as::<_, ObservationRow>(
            r#"SELECT id, workspace_id, place_id, observed_at, source,
                      source_url, collector_version, raw_activity_metrics,
                      observation_quality, created_at
               FROM community_observations
               WHERE workspace_id = $1 AND place_id = $2
               ORDER BY observed_at DESC
               LIMIT $3"#,
        )
        .bind(workspace_id)
        .bind(place_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(CommunityIntelligenceError::unexpected)
    }

    /// Returns the entities extracted in one observation (tenant-scoped).
    /// Tenant scoping is enforced by joining through community_observations
    /// with a workspace_id check.
    pub async fn entities_for_observation(
        &self,
        workspace_id: Uuid,
        observation_id: Uuid,
    ) -> Result<Vec<EntityRow>, CommunityIntelligenceError> {
        sqlx::query_as::<_, EntityRow>(
            r#"SELECT ce.id, ce.observation_id, ce.entity_type, ce.entity_ref,
                      ce.strength, ce.observed_at
               FROM community_entities ce
               JOIN community_observations co ON co.id = ce.observation_id
               WHERE co.workspace_id = $1 AND ce.observation_id = $2
               ORDER BY ce.strength DESC"#,
        )
        .bind(workspace_id)
        .bind(observation_id)
        .fetch_all(&self.pool)
        .await
        .map_err(CommunityIntelligenceError::unexpected)
    }

    /// Records where we stand with a community.
    ///
    /// Joining is a human act — the operator opens the forum, reads its rules,
    /// and asks to join under the band's own name. This is where they write
    /// down what happened, so the next person does not repeat it.
    ///
    /// # Errors
    /// Returns the underlying database error, or `NotFound` if the place does
    /// not belong to this workspace.
    pub async fn set_place_membership(
        &self,
        workspace_id: Uuid,
        place_id: Uuid,
        state: &str,
        note: Option<&str>,
        actor: &str,
    ) -> Result<bool, CommunityIntelligenceError> {
        let updated = sqlx::query(
            r#"UPDATE discovery_places
               SET membership_state = $3,
                   membership_note = $4,
                   membership_changed_at = now(),
                   membership_changed_by = $5,
                   updated_at = now()
               WHERE workspace_id = $1 AND id = $2"#,
        )
        .bind(workspace_id)
        .bind(place_id)
        .bind(state)
        .bind(note)
        .bind(actor)
        .execute(&self.pool)
        .await
        .map_err(CommunityIntelligenceError::unexpected)?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Returns all tracked places (forums) with their latest observation
    /// for a workspace. Uses a LEFT JOIN so places without observations
    /// still appear.
    pub async fn places_with_latest_observations(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<PlaceWithLatestObservation>, CommunityIntelligenceError> {
        sqlx::query_as::<_, PlaceWithLatestObservation>(
            r#"SELECT dp.id AS place_id, dp.place_kind, dp.platform, dp.name,
                      dp.url, dp.country_code, dp.language, dp.genres,
                      dp.member_count,
                      dp.membership_state, dp.membership_note,
                      dp.membership_changed_at,
                      co.id AS latest_observation_id,
                      co.observed_at AS latest_observed_at,
                      co.source AS latest_source,
                      co.observation_quality AS latest_observation_quality,
                      co.raw_activity_metrics AS latest_raw_activity_metrics
               FROM discovery_places dp
               LEFT JOIN LATERAL (
                   SELECT * FROM community_observations co2
                   WHERE co2.place_id = dp.id AND co2.workspace_id = $1
                   ORDER BY co2.observed_at DESC
                   LIMIT 1
               ) co ON true
               WHERE dp.workspace_id = $1
               -- Unjoined and largest first: the page should open on the
               -- community worth joining next, not on whichever name sorts
               -- first alphabetically.
               ORDER BY CASE dp.membership_state
                            WHEN 'not_joined' THEN 0
                            WHEN 'joining' THEN 1
                            WHEN 'joined' THEN 2
                            ELSE 3
                        END,
                        dp.member_count DESC NULLS LAST,
                        dp.name"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(CommunityIntelligenceError::unexpected)
    }
}
