//! Writing `FESTIVAL_PROFILE` rows into `festival_editions`.
//!
//! The sheet row knows a festival's name, its cycle status and — when the
//! cell carried `deadline=…` — the day its application window closes. The
//! booking graph knows which booking targets are festivals. This module is
//! the join: match by normalized name, upsert the edition on
//! `(target_id, edition_label)`, report whatever matched nothing.
//!
//! # What this never does
//!
//! - **Mint a booking target.** An unmatched festival is research the intake
//!   cannot yet anchor — reported, never guessed into `booking_targets`,
//!   where a wrong kind or a misspelled name would feed the deadline
//!   evaluator a room that does not exist.
//! - **Invent dates.** `application_opens_at` and `starts_at` stay NULL when
//!   the sheet does not say them; the close date is the only field the
//!   deadline lane reads, and it is the only one the sheet reliably carries.
//! - **Seed a past close.** `application_closes_at` must be a live window —
//!   the evaluator's own read already filters `>= now()`, and a row filed
//!   with a past date can only ever be dead weight.

use crowdrelay_domain::festival_seed::{FestivalSeedReport, FestivalSeedRow};
use sqlx::PgPool;
use time::{OffsetDateTime, Time};
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct FestivalSeedSummary {
    /// Editions newly inserted.
    pub seeded: u64,
    /// Editions whose `(target, label)` row already existed — refreshed.
    pub refreshed: u64,
    /// Sheet names that matched no `festival` booking target — reported so
    /// the operator sees the join's coverage rather than a silent drop.
    pub unmatched: Vec<String>,
    /// Rows whose window already closed — the sheet keeps last season's
    /// deadlines, and filing them would only ever be dead weight.
    pub past_deadline: u64,
    /// Rows whose own write failed — isolated per row.
    pub failed: u64,
}

#[derive(Clone)]
pub struct PostgresFestivalSeedRepository {
    pool: PgPool,
}

/// The name the sheet carries vs the name the target row carries: the
/// band's own display names append role suffixes ("… — organizator"),
/// the master's entity names do not. Normalize both sides the same way —
/// lowercase, drop the " — …" tail, collapse whitespace — so either side
/// carrying a descriptor still joins.
fn normalize_name(name: &str) -> String {
    let base = name
        .split('—')
        .next()
        .unwrap_or(name)
        .split(" - ")
        .next()
        .unwrap_or(name);
    base.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl PostgresFestivalSeedRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Writes each parsed row, per row. A bad row fails alone.
    ///
    /// `source_label` is `{file}#{sheet}` — the same provenance the
    /// outreach-log importer stamps.
    pub async fn import_sheet(
        &self,
        workspace_id: Uuid,
        source_label: &str,
        report: &FestivalSeedReport,
    ) -> Result<FestivalSeedSummary, sqlx::Error> {
        let mut summary = FestivalSeedSummary::default();
        for row in &report.rows {
            match self.seed_one(workspace_id, row).await {
                Ok(true) => summary.seeded += 1,
                Ok(false) => summary.refreshed += 1,
                Err(error) => {
                    if matches!(error, SeedError::NoTarget) {
                        summary.unmatched.push(row.entity_name.clone());
                    } else if matches!(error, SeedError::PastDeadline) {
                        summary.past_deadline += 1;
                    } else {
                        summary.failed += 1;
                        tracing::warn!(
                            %error,
                            festival = %row.entity_name,
                            source = %source_label,
                            "festival seed row failed to write"
                        );
                    }
                }
            }
        }
        Ok(summary)
    }

    /// Returns `true` on insert, `false` on refresh of the same edition.
    async fn seed_one(&self, workspace_id: Uuid, row: &FestivalSeedRow) -> Result<bool, SeedError> {
        let Some(deadline) = row.application_closes_on else {
            // The extractor guarantees this — defended here because the
            // guarantee is what makes the function honest.
            return Err(SeedError::NoDeadline);
        };
        let normalized = normalize_name(&row.entity_name);
        // The same normalization, in SQL, on the target side: btrim +
        // lowercase + the suffix cut are the two halves of one convention.
        let target_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id FROM booking_targets
            WHERE workspace_id = $1
              AND target_kind = 'festival'
              AND lower(btrim(regexp_replace(display_name, '\s*—.*$|\s+-\s+.*$', ''))) = $2
            ORDER BY created_at
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .bind(&normalized)
        .fetch_optional(&self.pool)
        .await
        .map_err(SeedError::Database)?
        .ok_or(SeedError::NoTarget)?;

        // The close timestamp is the end of the deadline's own day — a
        // festival that says "applications close 2026-10-10" accepts the
        // form on the tenth, not until its first minute.
        let closes_at = OffsetDateTime::new_utc(deadline, Time::MAX);
        if closes_at <= OffsetDateTime::now_utc() {
            return Err(SeedError::PastDeadline);
        }
        // The label is the window we actually know — "the window closing
        // then" — not an edition year the sheet never states. A corrected
        // deadline lands on the same label and refreshes; a *new* window is
        // a new label, which is what makes the upsert honest.
        let edition_label = format!("window closing {deadline}");
        // `festival_editions` carries no metadata column — provenance is the
        // intake counter log line plus the sheet itself, which stays in
        // Drive. `xmax = 0` distinguishes insert from refresh, the same
        // convention `upsert_seed` uses on booking agents.
        sqlx::query_scalar::<_, bool>(
            r#"
            INSERT INTO festival_editions (
                id, workspace_id, target_id, edition_label,
                application_closes_at, lineup_url
            ) VALUES ($1,$2,$3,$4,$5,$6)
            ON CONFLICT (workspace_id, target_id, edition_label) DO UPDATE SET
                application_closes_at = EXCLUDED.application_closes_at,
                lineup_url = COALESCE(EXCLUDED.lineup_url, festival_editions.lineup_url)
            RETURNING (xmax = 0)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(target_id)
        .bind(edition_label)
        .bind(closes_at)
        .bind(&row.website)
        .fetch_one(&self.pool)
        .await
        .map_err(SeedError::Database)
    }
}

enum SeedError {
    NoTarget,
    NoDeadline,
    PastDeadline,
    Database(sqlx::Error),
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTarget => write!(f, "no festival booking target matches"),
            Self::NoDeadline => write!(f, "row carries no deadline"),
            Self::PastDeadline => write!(f, "the window already closed"),
            Self::Database(e) => write!(f, "{e}"),
        }
    }
}

impl From<sqlx::Error> for SeedError {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}
