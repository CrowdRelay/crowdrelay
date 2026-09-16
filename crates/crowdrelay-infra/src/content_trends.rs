//! Trend persistence — the detection → storage half of the content engine.
//!
//! `PostgresContentEngineRepository` keeps the per-entity methods in
//! `content_engine.rs`; this module holds the detector orchestration:
//! read both observation tails, run the deterministic brain rules, upsert
//! the live rows, and fade what stopped appearing.
//!
//! The whole refresh runs inside ONE transaction under a per-workspace
//! advisory lock. The peer sweep and the community sweep can refresh the
//! same workspace concurrently; without the lock one refresh's fade could
//! mark a pattern the other just proved `faded` until the next pass, and
//! interleaved upserts open a deadlock window. Serialized, the refresh is
//! atomic: the trend table always reflects one consistent reading.

use sqlx::FromRow;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crowdrelay_domain::{
    ContentTrendId, WorkspaceId,
    content_engine::{ContentTrend, TrendDimension, TrendStatus},
};

use crate::content_engine::{ContentEngineError, PostgresContentEngineRepository, Result};

#[derive(Debug, FromRow)]
struct TrendRow {
    id: Uuid,
    workspace_id: Uuid,
    dimension: String,
    pattern: String,
    strength: i32,
    sources: i32,
    evidence: serde_json::Value,
    status: String,
    first_seen: Date,
    last_seen: Date,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<TrendRow> for ContentTrend {
    type Error = ContentEngineError;
    fn try_from(row: TrendRow) -> Result<Self> {
        Ok(Self {
            id: ContentTrendId::from_uuid(row.id),
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            dimension: TrendDimension::parse(&row.dimension)
                .ok_or(ContentEngineError::UnknownValue("trend dimension"))?,
            pattern: row.pattern,
            strength: row.strength,
            sources: row.sources,
            evidence: row.evidence,
            status: TrendStatus::parse(&row.status)
                .ok_or(ContentEngineError::UnknownValue("trend status"))?,
            first_seen: row.first_seen,
            last_seen: row.last_seen,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

impl PostgresContentEngineRepository {
    /// Reads both observation tails inside the detector's window, runs the
    /// deterministic rules over them, and upserts the live trends — one row
    /// per (dimension, pattern). A pattern absent from this pass fades
    /// rather than vanishing: the fade is a fact the operator should see.
    ///
    /// Returns the number of live trend rows this pass produced.
    pub async fn refresh_trends(&self, workspace_id: WorkspaceId, today: Date) -> Result<usize> {
        use crowdrelay_brain::content_trends::{TrendFact, WINDOW_DAYS, detect_trends};
        let ws = workspace_id.into_uuid();
        let cutoff = today - time::Duration::days(WINDOW_DAYS);

        let mut tx = self.pool.begin().await?;
        // One workspace, one refresh at a time — see the module docs for
        // the race this closes.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
            .bind(ws.to_string())
            .execute(&mut *tx)
            .await?;

        #[derive(FromRow)]
        struct FactRow {
            id: i64,
            source_key: Uuid,
            observed_at: Date,
            platform: String,
            fact: String,
        }
        let peer_rows = sqlx::query_as::<_, FactRow>(
            r#"
            SELECT id, peer_id AS source_key, observed_at, platform, fact
            FROM viryaos_peer_observations
            WHERE workspace_id = $1 AND observed_at >= $2 AND observed_at <= $3
            "#,
        )
        .bind(ws)
        .bind(cutoff)
        .bind(today)
        .fetch_all(&mut *tx)
        .await?;
        let fan_rows = sqlx::query_as::<_, FactRow>(
            r#"
            SELECT id, place_id AS source_key, observed_at, platform, fact
            FROM viryaos_fan_observations
            WHERE workspace_id = $1 AND observed_at >= $2 AND observed_at <= $3
            "#,
        )
        .bind(ws)
        .bind(cutoff)
        .bind(today)
        .fetch_all(&mut *tx)
        .await?;

        let facts: Vec<TrendFact> = peer_rows
            .iter()
            .map(|row| TrendFact {
                id: row.id,
                side: "peer",
                source_key: row.source_key,
                observed_at: row.observed_at,
                platform: &row.platform,
                fact: &row.fact,
            })
            .chain(fan_rows.iter().map(|row| TrendFact {
                id: row.id,
                side: "fan",
                source_key: row.source_key,
                observed_at: row.observed_at,
                platform: &row.platform,
                fact: &row.fact,
            }))
            .collect();
        let trends = detect_trends(&facts, today);

        for trend in &trends {
            sqlx::query(
                r#"
                INSERT INTO viryaos_content_trends (
                    id, workspace_id, dimension, pattern, strength, sources,
                    evidence, status, first_seen, last_seen
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                ON CONFLICT (workspace_id, dimension, pattern) DO UPDATE SET
                    strength = EXCLUDED.strength,
                    sources = EXCLUDED.sources,
                    evidence = EXCLUDED.evidence,
                    status = EXCLUDED.status,
                    -- The row's horizon only widens: a pattern rediscovered
                    -- after fading keeps its original first sighting.
                    first_seen = LEAST(viryaos_content_trends.first_seen, EXCLUDED.first_seen),
                    last_seen = GREATEST(viryaos_content_trends.last_seen, EXCLUDED.last_seen),
                    updated_at = now()
                "#,
            )
            .bind(ContentTrendId::new().into_uuid())
            .bind(ws)
            .bind(trend.dimension.as_str())
            .bind(&trend.pattern)
            .bind(trend.strength)
            .bind(trend.sources)
            .bind(&trend.evidence)
            .bind(trend.status.as_str())
            .bind(trend.first_seen)
            .bind(trend.last_seen)
            .execute(&mut *tx)
            .await?;
        }

        // Whatever did not appear in this pass is faded — history says so,
        // not silence. Two parallel arrays zip the seen (dimension, pattern)
        // pairs for the NOT EXISTS probe.
        let seen_dimensions: Vec<&str> = trends
            .iter()
            .map(|trend| trend.dimension.as_str())
            .collect();
        let seen_patterns: Vec<&str> = trends.iter().map(|trend| trend.pattern.as_str()).collect();
        sqlx::query(
            r#"
            UPDATE viryaos_content_trends
            SET status = 'faded', updated_at = now()
            WHERE workspace_id = $1
              AND status <> 'faded'
              AND NOT EXISTS (
                  SELECT 1
                  FROM unnest($2::text[], $3::text[]) AS seen(dimension, pattern)
                  WHERE seen.dimension = viryaos_content_trends.dimension
                    AND seen.pattern = viryaos_content_trends.pattern
              )
            "#,
        )
        .bind(ws)
        .bind(&seen_dimensions)
        .bind(&seen_patterns)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(trends.len())
    }

    /// The live and faded trends, strongest first — the read the suggestion
    /// engine (3.5b.5) and the Control Plane both hang off.
    pub async fn list_trends(&self, workspace_id: WorkspaceId) -> Result<Vec<ContentTrend>> {
        let rows = sqlx::query_as::<_, TrendRow>(
            r#"
            SELECT id, workspace_id, dimension, pattern, strength, sources,
                   evidence, status, first_seen, last_seen, created_at, updated_at
            FROM viryaos_content_trends
            WHERE workspace_id = $1
            ORDER BY strength DESC, pattern
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ContentTrend::try_from).collect()
    }
}
