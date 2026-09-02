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

        for entity in entities {
            sqlx::query(
                r#"INSERT INTO community_entities
                     (observation_id, entity_type, entity_ref, strength)
                   VALUES ($1, $2, $3, $4)"#,
            )
            .bind(row.id)
            .bind(entity.entity_type.as_db_str())
            .bind(&entity.entity_ref)
            .bind(entity.strength)
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
    pub async fn seconds_since_last_observation(
        &self,
    ) -> Result<Vec<(String, f64)>, CommunityIntelligenceError> {
        sqlx::query_as::<_, (String, f64)>(
            r#"SELECT source,
                      EXTRACT(EPOCH FROM (now() - max(observed_at)))::double precision
               FROM community_observations
               GROUP BY source"#,
        )
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
               ORDER BY dp.name"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(CommunityIntelligenceError::unexpected)
    }
}
