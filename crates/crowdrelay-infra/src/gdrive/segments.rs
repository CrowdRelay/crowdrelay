//! Archive segments — the review-queue cuts of `drive_contacts`.
//!
//! A 1,900-row staging table cannot be reviewed one row at a time, so the
//! queue is cut into five overlapping-free readings of the same rows: the
//! personal addresses that are probably fans, the organisation addresses a
//! person still has to read, the rows the sheet itself typed (beacons), the
//! rows a verification sheet marked inactive, and the rows the source
//! retracted. `decided` counts the remainder — rows a person already
//! promoted or dismissed — so the six numbers partition the whole table.
//!
//! Bulk promotion lives here too, and it is deliberate about who pulls the
//! trigger: the daily sync never promotes. Promotion happens when a person
//! clicks with a counted segment in front of them — the API refuses a click
//! whose confirmed count no longer matches the table. Consent is unchanged:
//! every promoted address still goes through `fan_import`'s pending +
//! double-opt-in path, and a suppressed address stays staged.

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::GDriveError;

/// A contact that can still become a fan — the columns `fan_import` needs
/// and nothing else.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StagedFanContact {
    pub id: Uuid,
    pub normalized_email: String,
    pub display_name: Option<String>,
    pub sources: Vec<String>,
    /// Raw city text from the sheet — the promote path resolves it through
    /// the catalogue (`staged_city_id` rules: one unambiguous match or
    /// nothing) before it reaches `ImportEntry.city_id`.
    pub city: Option<String>,
}

/// The shared base of the two undecided cuts: staged, untyped, present,
/// and not marked inactive.
///
/// The personal-mailbox providers a sheet full of fans is made of anchor
/// the split at `@(provider).` — a company-domain address is reviewed by
/// hand instead, because `promoter@club.pl` is a booking lead, not
/// somebody who followed the band. The `@` anchor keeps `gmail` inside a
/// local part or a lookalike domain (`gmail.evil.example`) from matching:
/// the cut is the provider, not a substring. `last_inbound_at` is the
/// second signal: an address that already wrote to the band is a fan
/// whatever provider it sits on.
///
/// A row whose sheet names an organisation is an organisation contact
/// whatever mailbox it uses, and that outranks both signals. Measured in
/// production on 2026-10-01: 699 of the 813 rows this cut returned named an
/// organisation — press, radio, festivals, playlist curators — because Polish
/// zines and promoters run on gmail and wp.pl, and the ones that "wrote to the
/// band" were replying to booking mail. Every one of them would have been sent
/// a fan opt-in invitation by the archive wave.
const LIKELY_FAN_PREDICATE: &str = "c.fan_outcome = 'staged' \
     AND c.suggested_kind IS NULL \
     AND c.disappeared_at IS NULL \
     AND c.staged_status IS DISTINCT FROM 'inactive' \
     AND NULLIF(btrim(c.organization), '') IS NULL \
     AND (c.normalized_email ~* '@(gmail|googlemail|wp|o2|onet|interia|hotmail|outlook|live|yahoo|icloud|me|op|tlen|poczta|gazeta|vp|proton|protonmail)\\.' \
          OR c.last_inbound_at IS NOT NULL)";

const LIKELY_ORG_PREDICATE: &str = "c.fan_outcome = 'staged' \
     AND c.suggested_kind IS NULL \
     AND c.disappeared_at IS NULL \
     AND c.staged_status IS DISTINCT FROM 'inactive' \
     AND (NULLIF(btrim(c.organization), '') IS NOT NULL \
          OR (c.normalized_email !~* '@(gmail|googlemail|wp|o2|onet|interia|hotmail|outlook|live|yahoo|icloud|me|op|tlen|poczta|gazeta|vp|proton|protonmail)\\.' \
              AND c.last_inbound_at IS NULL))";

/// How the review queue reads one staged row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactSegment {
    /// Personal-mailbox or already-corresponding addresses — the bulk
    /// promote's population.
    LikelyFan,
    /// Organisation domains with no inbound history — a person reviews
    /// these by hand.
    LikelyOrg,
    /// Rows the sheet itself typed: venue, press, radio, promoter,
    /// festival, agent.
    Beacon,
    /// A verification sheet marked the row inactive.
    Inactive,
    /// The source retracted the row.
    Gone,
}

impl ContactSegment {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LikelyFan => "likely_fan",
            Self::LikelyOrg => "likely_org",
            Self::Beacon => "beacon",
            Self::Inactive => "inactive",
            Self::Gone => "gone",
        }
    }

    /// `None` for a name the vocabulary does not know — the API turns that
    /// into a 400 rather than silently listing the whole queue.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "likely_fan" => Some(Self::LikelyFan),
            "likely_org" => Some(Self::LikelyOrg),
            "beacon" => Some(Self::Beacon),
            "inactive" => Some(Self::Inactive),
            "gone" => Some(Self::Gone),
            _ => None,
        }
    }

    /// The WHERE fragment over alias `c`, no bind parameters — the same
    /// text serves `list_contacts`' filter and `segment_counts`' FILTERs,
    /// so a count and the page it counts can never drift apart.
    #[must_use]
    pub fn sql_predicate(&self) -> &'static str {
        match self {
            Self::LikelyFan => LIKELY_FAN_PREDICATE,
            Self::LikelyOrg => LIKELY_ORG_PREDICATE,
            Self::Beacon => "c.suggested_kind IS NOT NULL",
            Self::Inactive => "c.staged_status = 'inactive'",
            Self::Gone => "c.disappeared_at IS NOT NULL",
        }
    }
}

/// The whole staging table in six numbers — computed over the table, never
/// inferred from a capped page.
#[derive(Debug, Default, Clone, Copy, serde::Serialize, sqlx::FromRow)]
pub struct SegmentCounts {
    pub likely_fan: i64,
    pub likely_org: i64,
    pub beacon: i64,
    pub inactive: i64,
    pub gone: i64,
    /// Rows a person already decided — promoted or dismissed on either
    /// outcome axis.
    pub decided: i64,
}

impl super::PostgresGDriveRepository {
    /// One pass over the staging table: every segment counted by the same
    /// predicate `list_contacts` filters on.
    pub async fn segment_counts(&self, workspace_id: Uuid) -> Result<SegmentCounts, GDriveError> {
        let sql = format!(
            "SELECT count(*) FILTER (WHERE {}) AS likely_fan, \
                    count(*) FILTER (WHERE {}) AS likely_org, \
                    count(*) FILTER (WHERE {}) AS beacon, \
                    count(*) FILTER (WHERE {}) AS inactive, \
                    count(*) FILTER (WHERE {}) AS gone, \
                    count(*) FILTER (WHERE c.fan_outcome <> 'staged' \
                                     AND c.beacon_outcome <> 'staged') AS decided \
             FROM drive_contacts c \
             WHERE c.workspace_id = $1",
            ContactSegment::LikelyFan.sql_predicate(),
            ContactSegment::LikelyOrg.sql_predicate(),
            ContactSegment::Beacon.sql_predicate(),
            ContactSegment::Inactive.sql_predicate(),
            ContactSegment::Gone.sql_predicate(),
        );
        sqlx::query_as::<_, SegmentCounts>(&sql)
            .bind(workspace_id)
            .fetch_one(self.pool())
            .await
            .map_err(GDriveError::Database)
    }

    /// The segment's promotable fan rows — `fan_outcome = 'staged'` always
    /// applies on top of the segment predicate, so `beacon` here means
    /// "typed rows still promotable as fans", not the whole beacon cut.
    ///
    /// `limit` bounds the wave: `None` returns the whole segment (the
    /// one-click bulk promote), `Some(n)` the top `n` by evidence — an
    /// address that already wrote to the band outranks a dual-source
    /// sighting, which outranks a bare row. Evidence order makes a capped
    /// wave promote the contacts most likely to be real fans, not an
    /// arbitrary slice.
    pub async fn staged_fan_contacts_in_segment(
        &self,
        workspace_id: Uuid,
        segment: ContactSegment,
        limit: Option<i64>,
    ) -> Result<Vec<StagedFanContact>, GDriveError> {
        let sql = format!(
            "SELECT c.id, c.normalized_email, c.display_name, c.sources, c.city \
             FROM drive_contacts c \
             WHERE c.workspace_id = $1 AND c.fan_outcome = 'staged' \
               AND ({}) \
             ORDER BY (c.last_inbound_at IS NOT NULL) DESC, \
                      (cardinality(c.sources) > 1) DESC, \
                      (c.city IS NOT NULL AND btrim(c.city) <> '') DESC, \
                      c.id \
             LIMIT $2",
            segment.sql_predicate(),
        );
        sqlx::query_as::<_, StagedFanContact>(&sql)
            .bind(workspace_id)
            .bind(limit.unwrap_or(i64::MAX))
            .fetch_all(self.pool())
            .await
            .map_err(GDriveError::Database)
    }

    /// The segment's promotable beacon rows — `beacon_outcome = 'staged'`
    /// always applies on top of the segment predicate, so `likely_fan`
    /// here means "fan-typed rows still promotable as beacons". Same
    /// evidence ordering and `limit` semantics as the fan sibling: a
    /// capped wave takes the contacts most likely to be real targets,
    /// and the full `DriveContactRow` comes back because every kind of
    /// promote (outreach, booking, agent, representation) reads
    /// different columns of it.
    pub async fn staged_beacon_contacts_in_segment(
        &self,
        workspace_id: Uuid,
        segment: ContactSegment,
        limit: Option<i64>,
    ) -> Result<Vec<super::DriveContactRow>, GDriveError> {
        let sql = format!(
            "{SELECT} \
             WHERE c.workspace_id = $1 AND c.beacon_outcome = 'staged' \
               AND ({predicate}) \
             ORDER BY (c.last_inbound_at IS NOT NULL) DESC, \
                      (cardinality(c.sources) > 1) DESC, \
                      (c.city IS NOT NULL AND btrim(c.city) <> '') DESC, \
                      c.id \
             LIMIT $2",
            SELECT = super::CONTACT_SELECT,
            predicate = segment.sql_predicate(),
        );
        sqlx::query_as::<_, super::DriveContactRow>(&sql)
            .bind(workspace_id)
            .bind(limit.unwrap_or(i64::MAX))
            .fetch_all(self.pool())
            .await
            .map_err(GDriveError::Database)
    }

    /// The segment's live promotable count — the `expected_count` check
    /// counts the table as it stands, not the page a wave would take.
    pub async fn staged_fan_count_in_segment(
        &self,
        workspace_id: Uuid,
        segment: ContactSegment,
    ) -> Result<i64, GDriveError> {
        let sql = format!(
            "SELECT count(*) FROM drive_contacts c \
             WHERE c.workspace_id = $1 AND c.fan_outcome = 'staged' \
               AND ({})",
            segment.sql_predicate(),
        );
        sqlx::query_scalar::<_, i64>(&sql)
            .bind(workspace_id)
            .fetch_one(self.pool())
            .await
            .map_err(GDriveError::Database)
    }

    /// Resolves sheet city texts through the catalogue, applying the same
    /// uniqueness rule as `staged_city_id` — a text matching two different
    /// cities resolves to nothing rather than guessing. One round trip for
    /// a whole wave's distinct texts; keys are the lowercased inputs.
    pub async fn staged_city_ids(
        &self,
        cities: &[String],
    ) -> Result<std::collections::HashMap<String, Uuid>, GDriveError> {
        let cleaned: Vec<String> = cities
            .iter()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty())
            .collect();
        let mut resolved: std::collections::HashMap<String, Uuid> =
            std::collections::HashMap::new();
        if cleaned.is_empty() {
            return Ok(resolved);
        }
        let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "SELECT id, slug, lower(btrim(name)) FROM cities \
             WHERE slug = ANY($1) OR lower(btrim(name)) = ANY($1)",
        )
        .bind(&cleaned)
        .fetch_all(self.pool())
        .await
        .map_err(GDriveError::Database)?;
        for text in cleaned {
            let mut ids: Vec<Uuid> = rows
                .iter()
                .filter(|(_, slug, name)| slug == &text || name == &text)
                .map(|(id, _, _)| *id)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            if let [id] = ids.as_slice() {
                resolved.insert(text, *id);
            }
        }
        Ok(resolved)
    }

    /// Marks the listed contacts' fan outcome promoted inside the caller's
    /// transaction — a bulk promote imports and marks atomically, so a mark
    /// failure can never leave a pending fan whose staging row still reads
    /// `staged`. Rows that left `staged` between the count and the write are
    /// not touched; the caller sees the real marked count, not the list's
    /// length.
    pub async fn mark_fans_promoted_by_ids(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        ids: &[Uuid],
    ) -> Result<u64, GDriveError> {
        let result = sqlx::query(
            "UPDATE drive_contacts SET fan_outcome = 'promoted' \
             WHERE workspace_id = $1 AND id = ANY($2) AND fan_outcome = 'staged'",
        )
        .bind(workspace_id)
        .bind(ids)
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected())
    }
}
