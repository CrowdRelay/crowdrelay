//! Writing scout `OPPORTUNITIES` sheets into `team_opportunities`.
//!
//! `team_opportunities` is what the `booking_opportunity` and
//! `live_opportunity` contexts rank and act on. Until now rows entered it
//! two ways — the one-shot CSV import (`crm:zgloszenia`) and the agent
//! scout's findings (`agent_scout`). This is the third lane: the sheets the
//! band's scout workbooks actually maintain — `VIRYA_MASTER`, `SCOUT`,
//! `SCOUT AUTO`, and the GitHub `database_festivals.xlsx` snapshot — read
//! by `crowdrelay_domain::opportunity_seed` and upserted here.
//!
//! Re-running is safe and cross-file dedupe is the point. `source` is one
//! constant (`scout_sheet`) and `external_key` is the destination URL or
//! the name+city slug, so the same opportunity asserted by MASTER, the
//! SCOUT review queue and the GitHub mirror lands on ONE row instead of
//! three. The row's own `Dedupe_Key`/`Opportunity_ID` travels in
//! `metadata` for reconciliation but is never the conflict key — each
//! sheet mints identifiers in its own namespace.
//!
//! The conflict arm copies `import_opportunities.rs` semantics exactly:
//! contact details, deadlines and scores refresh; `status` and `eligible`
//! never do — a stale snapshot must not reopen a row the loop already acted on.
//! Registry metadata is merged additively with existing runtime metadata winning
//! on key collisions, so new evidence/dedupe fields can enrich an old row without
//! erasing operator/runtime corrections.

use crowdrelay_domain::opportunity_seed::SeededOpportunity;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::{Date, OffsetDateTime, Time, UtcOffset};
use uuid::Uuid;

/// The source every scout-sheet row carries. Half of the unique key, so
/// it is a constant rather than a file name: a MASTER row and a
/// `database_festivals.xlsx` row for the same opportunity must collide,
/// and they only collide inside one source lane.
const SOURCE: &str = "scout_sheet";

/// What one sheet's import did, so the cycle report is honest about
/// "seen" vs "new".
#[derive(Debug, Default)]
pub struct OpportunitySeedSummary {
    /// Rows newly inserted into `team_opportunities`.
    pub seeded: u64,
    /// Rows whose conflict key already existed — refreshed in place.
    pub refreshed: u64,
    /// Rows whose own write failed — isolated per row so one bad cell
    /// does not abort the sheet.
    pub failed: u64,
}

/// Sheet dates are days, not instants — anchored to midnight UTC the same
/// way `import_opportunities` anchors them, so the deadline is treated as
/// passed from the start of that day.
fn at_midnight(date: Date) -> OffsetDateTime {
    OffsetDateTime::new_in_offset(date, Time::MIDNIGHT, UtcOffset::UTC)
}

/// Everything the row knows that the table has no column for, plus the
/// provenance needed to reconcile it: which file asserted the row and
/// which grammar parsed it. `status`, `eligible` and this map are written
/// on insert only — the update arm below never touches them.
fn metadata(row: &SeededOpportunity, source_file: &str) -> Value {
    let mut map = match &row.metadata {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    map.insert("import_source".into(), json!(SOURCE));
    map.insert("import_file".into(), json!(source_file));
    if let Some(city) = &row.city {
        map.insert("city".into(), json!(city));
    }
    if row.raw_kind != row.kind {
        map.insert("raw_kind".into(), json!(&row.raw_kind));
    }
    Value::Object(map)
}

/// The repository: one pool, per-row writes.
pub struct PostgresOpportunitySeedRepository {
    pool: PgPool,
}

impl PostgresOpportunitySeedRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Imports every parsed row of one sheet. A row that fails its own
    /// write is counted and logged, not fatal — one malformed cell must
    /// not cost the sheet's other findings.
    pub async fn import_sheet(
        &self,
        workspace_id: Uuid,
        source_file: &str,
        rows: &[SeededOpportunity],
    ) -> Result<OpportunitySeedSummary, sqlx::Error> {
        let mut summary = OpportunitySeedSummary::default();
        // Deterministic write order — two files arriving the same minute
        // upsert the same key, so last-write ordering should not depend on
        // whatever order the sheet happened to keep its rows in.
        let mut rows: Vec<&SeededOpportunity> = rows.iter().collect();
        rows.sort_by(|a, b| a.external_key.cmp(&b.external_key));
        for row in rows {
            match self.import_row(workspace_id, source_file, row).await {
                Ok(inserted) => {
                    if inserted {
                        summary.seeded += 1;
                    } else {
                        summary.refreshed += 1;
                    }
                }
                Err(error) => {
                    summary.failed += 1;
                    tracing::warn!(
                        %error,
                        file = %source_file,
                        external_key = %row.external_key,
                        title = %row.title,
                        "opportunity seed row failed to write"
                    );
                }
            }
        }
        Ok(summary)
    }

    /// One row's upsert. Returns whether it inserted (vs refreshed) — the
    /// `xmax = 0` tag is the only way RETURNING can say "new" rather than
    /// "seen again".
    async fn import_row(
        &self,
        workspace_id: Uuid,
        source_file: &str,
        row: &SeededOpportunity,
    ) -> Result<bool, sqlx::Error> {
        // The link is dated source evidence: a row carrying one was
        // observed at import time, a row without one was never observed.
        let observed_at: Option<OffsetDateTime> =
            row.destination_url.is_some().then(OffsetDateTime::now_utc);
        sqlx::query_scalar::<_, bool>(
            r#"
            INSERT INTO team_opportunities
                (workspace_id, opportunity_kind, source, external_key, title,
                 organization, destination_url, contact_email, country_code,
                 fit_basis_points, confidence_basis_points,
                 strategic_value_basis_points, verified_destination,
                 deadline, event_starts_at, eligible, metadata, status,
                 source_observed_at)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,'new',$18)
            ON CONFLICT (workspace_id, source, external_key) DO UPDATE SET
                -- Contact details and dates are what a re-import is for:
                -- the sheet is the band's live record and CrowdRelay's
                -- copy goes stale. COALESCE so a blanked cell does not
                -- erase what we have.
                opportunity_kind = EXCLUDED.opportunity_kind,
                title = EXCLUDED.title,
                organization = EXCLUDED.organization,
                contact_email = COALESCE(EXCLUDED.contact_email, team_opportunities.contact_email),
                destination_url = COALESCE(EXCLUDED.destination_url, team_opportunities.destination_url),
                country_code = COALESCE(EXCLUDED.country_code, team_opportunities.country_code),
                deadline = COALESCE(EXCLUDED.deadline, team_opportunities.deadline),
                event_starts_at = COALESCE(EXCLUDED.event_starts_at, team_opportunities.event_starts_at),
                fit_basis_points = EXCLUDED.fit_basis_points,
                confidence_basis_points = EXCLUDED.confidence_basis_points,
                -- The sheet fills a blank strategic value but never
                -- overwrites one: a nonzero value may be operator
                -- judgement or something the loop learned.
                strategic_value_basis_points = CASE
                    WHEN team_opportunities.strategic_value_basis_points = 0
                        THEN EXCLUDED.strategic_value_basis_points
                    ELSE team_opportunities.strategic_value_basis_points
                END,
                -- Verification promotes false to true, never the reverse.
                verified_destination = team_opportunities.verified_destination
                    OR EXCLUDED.verified_destination,
                source_observed_at = COALESCE(
                    EXCLUDED.source_observed_at,
                    team_opportunities.source_observed_at),
                -- Status and eligibility are the loop/operator's record and
                -- never reopen from a registry refresh. Metadata does enrich:
                -- registry keys fill gaps, while existing runtime/operator keys
                -- win collisions so a sheet cannot erase learned state.
                metadata = EXCLUDED.metadata || team_opportunities.metadata,
                updated_at = now(),
                version = team_opportunities.version + 1
            RETURNING (xmax = 0)
            "#,
        )
        .bind(workspace_id)
        .bind(row.kind)
        .bind(SOURCE)
        .bind(row.external_key.trim())
        .bind(row.title.trim())
        .bind(row.organization.trim())
        .bind(row.destination_url.as_deref())
        .bind(row.contact_email.as_deref())
        .bind(row.country_code.as_deref())
        .bind(row.fit_basis_points)
        .bind(row.confidence_basis_points)
        .bind(row.strategic_value_basis_points)
        .bind(row.verified_destination)
        .bind(row.deadline.map(at_midnight))
        .bind(row.event_starts_on.map(at_midnight))
        .bind(row.eligible)
        .bind(metadata(row, source_file))
        .bind(observed_at)
        .fetch_one(&self.pool)
        .await
    }
}
