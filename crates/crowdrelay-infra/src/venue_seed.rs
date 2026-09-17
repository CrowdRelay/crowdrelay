//! Writing a researched venue sheet into the shared registry.
//!
//! `domain::venue_seed` splits a sheet row into the room everyone sees and
//! the view one tenant holds; this module is where that split lands on
//! disk. The room upserts into `place_venues` — a pure identity row — and
//! everything the sheet claims about it becomes a `place_venue_facts` row
//! with `provenance = 'researched'` and the sheet's own `Source_URL` as
//! `source_ref`.
//!
//! # Which facts are global
//!
//! `workspace_id NULL` is the whole platform's claim about the room:
//! capacity, genres, website, address, a published rate card, a closed
//! status, the country it sits in. `workspace_id` set is the contributing
//! tenant's private knowledge: the booking address it found, the terms
//! search it ran and came back empty, and its own fit judgement — contact
//! and money are never global, because that is exactly what a competitor
//! would like to read.
//!
//! A `"closed"` status is written only when the sheet marks the room
//! closed — "active" is never a fact, because the absence of a closed
//! record already IS the active claim, and writing both would let a stale
//! "active" outlive a newer "closed".
//!
//! # What a re-scan does and does not do
//!
//! Facts upsert on `(venue_id, attribute, provenance, source_ref)` — a
//! re-researched row refreshes the claim's value and clock rather than
//! stacking a twin. But a row that *vanishes* from a re-scan deletes
//! nothing: a researched fact lives until re-researched, because its
//! source is the research itself. A gap in the next sheet is a gap in the
//! sheet, not a retraction of the claim.

use crowdrelay_domain::venue_seed::{PublicTerms, SeedSheetReport, SeededVenue};
use sqlx::PgPool;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// What one venue's import did. `UnknownCity` is an honest refusal, not a
/// guess: a room whose city is not in the catalogue is counted and skipped
/// rather than filed against a place it may not be.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SeedOutcome {
    Imported,
    UnknownCity,
}

/// What one sheet's import did, in counts the worker can report.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct VenueSeedSummary {
    pub imported: u64,
    /// Rows whose city could not be resolved against `cities`. Not an
    /// error — the sheet is fixable and the venues stay out, not misfiled.
    pub unknown_city: u64,
}

#[derive(Clone)]
pub struct PostgresVenueSeedRepository {
    pool: PgPool,
}

impl PostgresVenueSeedRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Imports every parsed venue of one sheet. Each venue is its own
    /// transaction — one bad row must not take the sheet's other rooms
    /// with it, and a half-written venue is worse than a refused one.
    pub async fn import_sheet(
        &self,
        workspace_id: Uuid,
        report: &SeedSheetReport,
    ) -> Result<VenueSeedSummary, sqlx::Error> {
        let mut summary = VenueSeedSummary::default();
        for venue in &report.venues {
            match self.import_venue(workspace_id, venue).await? {
                SeedOutcome::Imported => summary.imported += 1,
                SeedOutcome::UnknownCity => summary.unknown_city += 1,
            }
        }
        Ok(summary)
    }

    /// One venue: resolve the city, upsert the room, write its facts. The
    /// city is resolved the way `gdrive::promote_beacon_booking` resolves
    /// it — slug first, then a name match only when exactly one city
    /// answers, because a name two cities share picks nothing rather than
    /// picking wrong.
    async fn import_venue(
        &self,
        workspace_id: Uuid,
        venue: &SeededVenue,
    ) -> Result<SeedOutcome, sqlx::Error> {
        let room = &venue.room;
        let slug = match sqlx::query_scalar::<_, String>(
            "SELECT slug FROM cities WHERE slug = lower(btrim($1))",
        )
        .bind(&room.city)
        .fetch_optional(&self.pool)
        .await?
        {
            Some(slug) => Some(slug),
            None => {
                let slugs = sqlx::query_scalar::<_, String>(
                    "SELECT DISTINCT slug FROM cities \
                     WHERE slug = lower(btrim($1)) \
                        OR lower(btrim(name)) = lower(btrim($1))",
                )
                .bind(&room.city)
                .fetch_all(&self.pool)
                .await?;
                match slugs.as_slice() {
                    [only] => Some(only.clone()),
                    _ => None,
                }
            }
        };
        let Some(slug) = slug else {
            return Ok(SeedOutcome::UnknownCity);
        };
        let Some(city_id) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities WHERE slug = $1")
            .bind(&slug)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(SeedOutcome::UnknownCity);
        };

        // The fact's clock is the sheet's own Research_Date at midnight
        // UTC; a missing or unparsable one means "seen now" — a date the
        // sheet wrote in another shape is not a parsing error the import
        // should die on.
        let date_format = time::macros::format_description!("[year]-[month]-[day]");
        let observed_at: Option<OffsetDateTime> = room
            .researched_on
            .as_deref()
            .map(str::trim)
            .and_then(|raw| Date::parse(raw, &date_format).ok())
            .map(|date| date.midnight().assume_utc());

        let mut tx = self.pool.begin().await?;
        let venue_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO place_venues (city_id, name_key, display_name)
            VALUES ($1, place_venue_key($2), left(btrim($3), 500))
            ON CONFLICT (city_id, name_key)
            DO UPDATE SET display_name = EXCLUDED.display_name
            RETURNING id
            "#,
        )
        .bind(city_id)
        .bind(&room.name)
        .bind(&room.name)
        .fetch_one(&mut *tx)
        .await?;

        // Global half — the room as everyone sees it (workspace_id NULL).
        if let Some(capacity) = room.capacity {
            write_fact(
                &mut tx,
                venue_id,
                "capacity",
                &capacity.to_string(),
                &room.source_url,
                observed_at,
                None,
            )
            .await?;
        }
        if !room.genre_tags.is_empty() {
            write_fact(
                &mut tx,
                venue_id,
                "genres",
                &room.genre_tags.join(", "),
                &room.source_url,
                observed_at,
                None,
            )
            .await?;
        }
        if let Some(website) = &room.website {
            write_fact(
                &mut tx,
                venue_id,
                "website",
                website,
                &room.source_url,
                observed_at,
                None,
            )
            .await?;
        }
        if let Some(address) = &room.address {
            write_fact(
                &mut tx,
                venue_id,
                "address",
                address,
                &room.source_url,
                observed_at,
                None,
            )
            .await?;
        }
        // Only "closed" is ever written — see the module header.
        if room.closed {
            write_fact(
                &mut tx,
                venue_id,
                "status",
                "closed",
                &room.source_url,
                observed_at,
                None,
            )
            .await?;
        }
        if !room.country.trim().is_empty() {
            write_fact(
                &mut tx,
                venue_id,
                "country",
                room.country.trim(),
                &room.source_url,
                observed_at,
                None,
            )
            .await?;
        }
        // The one terms result that *may* be global — and only when the
        // source is the room's own site.
        //
        // The column is called Public_Financial_Info and the brief asks for
        // what the venue publishes, but a researcher types what they have:
        // "they quote 30% of the door" is somebody's private negotiating
        // position, and publishing it to every tenant is exactly what §4h-9
        // forbids. So the claim is global when the row's own source link sits
        // on the same host as the venue's website — the room saying it in
        // public — and contributor-private otherwise. Nothing is lost either
        // way; the scope is the difference between a fact about the room and
        // a fact one tenant learned.
        if let PublicTerms::Published(text) = &room.public_terms {
            let published_by_the_room = room
                .website
                .as_deref()
                .is_some_and(|website| same_host(website, &room.source_url));
            write_fact(
                &mut tx,
                venue_id,
                "public_terms",
                text,
                &room.source_url,
                observed_at,
                if published_by_the_room {
                    None
                } else {
                    Some(workspace_id)
                },
            )
            .await?;
        }

        // Private half — one tenant's knowledge and judgement.
        if let Some(email) = &room.booking_email {
            write_fact(
                &mut tx,
                venue_id,
                "booking_email",
                email,
                &room.source_url,
                observed_at,
                Some(workspace_id),
            )
            .await?;
        }
        if matches!(room.public_terms, PublicTerms::SearchedNoneFound) {
            write_fact(
                &mut tx,
                venue_id,
                "public_terms",
                "searched: none found",
                &room.source_url,
                observed_at,
                Some(workspace_id),
            )
            .await?;
        }
        if let Some(target_fit) = &venue.view.target_fit {
            write_fact(
                &mut tx,
                venue_id,
                "target_fit",
                target_fit,
                &room.source_url,
                observed_at,
                Some(workspace_id),
            )
            .await?;
        }
        if let Some(angle) = &venue.view.outreach_angle {
            write_fact(
                &mut tx,
                venue_id,
                "outreach_angle",
                angle,
                &room.source_url,
                observed_at,
                Some(workspace_id),
            )
            .await?;
        }
        if let Some(quality) = &venue.view.contact_quality {
            write_fact(
                &mut tx,
                venue_id,
                "contact_quality",
                quality,
                &room.source_url,
                observed_at,
                Some(workspace_id),
            )
            .await?;
        }
        if let Some(notes) = &venue.view.notes {
            write_fact(
                &mut tx,
                venue_id,
                "notes",
                notes,
                &room.source_url,
                observed_at,
                Some(workspace_id),
            )
            .await?;
        }

        tx.commit().await?;
        Ok(SeedOutcome::Imported)
    }
}

/// Whether two URLs name the same host, ignoring scheme, `www.` and case.
///
/// Used to decide whether a terms claim came from the room's own page. A
/// comparison that cannot parse either side answers `false`: the scope falls
/// back to private, which is the direction that cannot leak.
fn same_host(left: &str, right: &str) -> bool {
    fn host(url: &str) -> Option<String> {
        let rest = url
            .trim()
            .to_lowercase()
            .split_once("//")
            .map_or_else(|| url.trim().to_lowercase(), |(_, rest)| rest.to_owned());
        let host = rest.split('/').next()?.split('@').next_back()?;
        let host = host.split(':').next()?;
        let host = host.strip_prefix("www.").unwrap_or(host);
        (!host.is_empty()).then(|| host.to_owned())
    }
    match (host(left), host(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// One attributed claim. `workspace_id` `None` writes the global row;
/// `Some` writes the contributor-private one. Value and source_ref are
/// capped at the CHECK bounds (2000) — a sheet cell longer than that is
/// truncated rather than failing the venue's whole import over prose.
async fn write_fact(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    venue_id: Uuid,
    attribute: &str,
    value: &str,
    source_ref: &str,
    observed_at: Option<OffsetDateTime>,
    workspace_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    let value: String = value.chars().take(2000).collect();
    let source_ref: String = source_ref.chars().take(2000).collect();
    // Two partial unique indexes back the dedupe (see migration 0308): a
    // global fact conflicts on the four-column key, a private one on the
    // key plus its workspace. One statement per scope — the arbiter's WHERE
    // must match the index predicate for inference to find it.
    let sql = if workspace_id.is_some() {
        r#"
        INSERT INTO place_venue_facts
            (venue_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
        VALUES ($1, $2, $3, 'researched', $4, COALESCE($5, now()), $6)
        ON CONFLICT (venue_id, attribute, provenance, source_ref, workspace_id)
        WHERE workspace_id IS NOT NULL
        DO UPDATE SET value = EXCLUDED.value, observed_at = EXCLUDED.observed_at
        "#
    } else {
        r#"
        INSERT INTO place_venue_facts
            (venue_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
        VALUES ($1, $2, $3, 'researched', $4, COALESCE($5, now()), NULL)
        ON CONFLICT (venue_id, attribute, provenance, source_ref)
        WHERE workspace_id IS NULL
        DO UPDATE SET value = EXCLUDED.value, observed_at = EXCLUDED.observed_at
        "#
    };
    let mut query = sqlx::query(sql)
        .bind(venue_id)
        .bind(attribute)
        .bind(&value)
        .bind(&source_ref)
        .bind(observed_at);
    // $6 exists only in the private statement — Postgres refuses a bind
    // the SQL does not name, so it is appended, not bound unconditionally.
    if let Some(workspace_id) = workspace_id {
        query = query.bind(workspace_id);
    }
    query.execute(&mut **tx).await?;
    Ok(())
}
