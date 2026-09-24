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
//! A `"closed"` or `"active"` status is written verbatim when the sheet
//! carries it — the active claim exists so a *reopened* room can outrank
//! its stale closed record: same provenance, newer `observed_at`, and the
//! resolved-status read lifts the exclusion. Anything else in the column
//! writes nothing, because an unrecognized liveness claim must not become
//! a fact.
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
    /// Rows whose write failed — the row is counted and skipped so one bad
    /// cell does not take the sheet's other rooms with it.
    pub failed: u64,
}

/// One attributed claim about a room, as `write_fact` takes it — a struct
/// rather than nine positional arguments because the licence and the
/// deletion clock are exactly the parameters a caller should have to name.
pub struct VenueFactWrite<'a> {
    pub venue_id: Uuid,
    pub attribute: &'a str,
    pub value: &'a str,
    pub provenance: &'a str,
    pub source_ref: &'a str,
    pub observed_at: Option<OffsetDateTime>,
    pub expires_at: Option<OffsetDateTime>,
    pub workspace_id: Option<Uuid>,
    pub licence: Option<&'a str>,
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

    /// One attributed claim about a room — the single upsert every fact
    /// writer goes through, so the sheet importer and the open-directory
    /// sweeps share one dedupe rule rather than two copies of it.
    ///
    /// `workspace_id` `None` writes the global row; `Some` writes the
    /// contributor-private one. `provenance` names the fact's trust class
    /// (the CHECK constraint on the column is the arbiter — an unknown class
    /// fails the write rather than being filed silently). `expires_at` is a
    /// deletion deadline, not a staleness hint: the hourly expiry sweep
    /// deletes the row once it passes. `licence` is `Some("odbl")` for
    /// OSM-derived facts — share-alike means the licence travels with the
    /// fact — and `None` for everything else. A refresh of the same
    /// (venue, attribute, provenance, source_ref, scope) claim rewrites the
    /// value, the clock, the deadline and the licence together, so a
    /// re-sweep updates the row rather than stacking a twin.
    ///
    /// Value and source_ref are capped at the CHECK bounds (2000) — a
    /// source cell longer than that is truncated rather than failing the
    /// venue's whole write over prose.
    ///
    /// # Errors
    ///
    /// Propagates the database error — including the CHECK violations a
    /// caller earns for an unknown provenance, an over-long attribute, or a
    /// licence other than `odbl`.
    pub async fn write_fact<'e, E>(executor: E, fact: VenueFactWrite<'_>) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let value: String = fact.value.chars().take(2000).collect();
        let source_ref: String = fact.source_ref.chars().take(2000).collect();
        // Two partial unique indexes back the dedupe (see migration 0308): a
        // global fact conflicts on the four-column key, a private one on the
        // key plus its workspace. One statement per scope — the arbiter's
        // WHERE must match the index predicate for inference to find it.
        let sql = if fact.workspace_id.is_some() {
            r#"
            INSERT INTO place_venue_facts
                (venue_id, attribute, value, provenance, source_ref,
                 observed_at, expires_at, workspace_id, licence)
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()), $7, $9, $8)
            ON CONFLICT (venue_id, attribute, provenance, source_ref, workspace_id)
            WHERE workspace_id IS NOT NULL
            DO UPDATE SET value = EXCLUDED.value,
                          observed_at = EXCLUDED.observed_at,
                          expires_at = EXCLUDED.expires_at,
                          licence = EXCLUDED.licence
            "#
        } else {
            r#"
            INSERT INTO place_venue_facts
                (venue_id, attribute, value, provenance, source_ref,
                 observed_at, expires_at, workspace_id, licence)
            VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()), $7, NULL, $8)
            ON CONFLICT (venue_id, attribute, provenance, source_ref)
            WHERE workspace_id IS NULL
            DO UPDATE SET value = EXCLUDED.value,
                          observed_at = EXCLUDED.observed_at,
                          expires_at = EXCLUDED.expires_at,
                          licence = EXCLUDED.licence
            "#
        };
        let mut query = sqlx::query(sql)
            .bind(fact.venue_id)
            .bind(fact.attribute)
            .bind(&value)
            .bind(fact.provenance)
            .bind(&source_ref)
            .bind(fact.observed_at)
            .bind(fact.expires_at)
            .bind(fact.licence);
        // $9 exists only in the private statement — Postgres refuses a bind
        // the SQL does not name, so it is appended, not bound unconditionally.
        if let Some(workspace_id) = fact.workspace_id {
            query = query.bind(workspace_id);
        }
        query.execute(executor).await?;
        Ok(())
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
            match self.import_venue(workspace_id, venue).await {
                Ok(SeedOutcome::Imported) => summary.imported += 1,
                Ok(SeedOutcome::UnknownCity) => summary.unknown_city += 1,
                Err(error) => {
                    summary.failed += 1;
                    tracing::warn!(
                        %error,
                        venue = %venue.room.name,
                        "venue seed row refused"
                    );
                }
            }
        }
        Ok(summary)
    }

    /// One venue: resolve the city, upsert the room, write its facts. The
    /// city resolves the way `peer_act_seed::import_act` resolves it — a
    /// name or slug match constrained by the sheet's country when it maps,
    /// accepted only when exactly one city answers, because a city two
    /// countries share picks nothing rather than picking wrong.
    async fn import_venue(
        &self,
        workspace_id: Uuid,
        venue: &SeededVenue,
    ) -> Result<SeedOutcome, sqlx::Error> {
        let room = &venue.room;
        // `cities.slug` is unique per country, not globally — the id resolves
        // in the same query the sheet's country constraint ran in, and the
        // match must be exactly one: a "Neustadt" inside a Czechia row must
        // not resolve to the German catalogue entry, and a slug two countries
        // share must not pick one by accident. A country the map does not
        // know constrains nothing — the exactly-one rule still stands.
        let country_code = crate::peer_act_seed::resolve_country_code(room.country.trim());
        let ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM cities \
             WHERE (slug = lower(btrim($1)) \
                OR lower(btrim(name)) = lower(btrim($1))) \
               AND ($2::text IS NULL OR country_code = $2)",
        )
        .bind(&room.city)
        .bind(country_code)
        .fetch_all(&self.pool)
        .await?;
        let Some(city_id) = (match ids.as_slice() {
            [only] => Some(*only),
            _ => None,
        }) else {
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
            researched_fact(
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
            researched_fact(
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
            researched_fact(
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
            researched_fact(
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
        // The sheet's liveness claim, verbatim: "closed" marks the room so
        // it is not researched twice, and "active" is written too — a fresh
        // `active` is how a reopened room's claim outranks its stale
        // `closed` on `observed_at` within the same `researched` provenance.
        if let Some(status) = &room.status {
            researched_fact(
                &mut tx,
                venue_id,
                "status",
                status,
                &room.source_url,
                observed_at,
                None,
            )
            .await?;
        }
        if !room.country.trim().is_empty() {
            researched_fact(
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
            researched_fact(
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
            researched_fact(
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
            researched_fact(
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
            researched_fact(
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
            researched_fact(
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
            researched_fact(
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
            researched_fact(
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

/// The sheet's own claims: `researched` provenance, no deletion clock, no
/// licence. The shorthand exists so the import above reads as the list of
/// attributes it writes rather than a wall of repeated constants — the
/// upsert itself is `PostgresVenueSeedRepository::write_fact`, which every
/// fact writer (this one included) shares.
async fn researched_fact(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    venue_id: Uuid,
    attribute: &str,
    value: &str,
    source_ref: &str,
    observed_at: Option<OffsetDateTime>,
    workspace_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    PostgresVenueSeedRepository::write_fact(
        &mut **tx,
        VenueFactWrite {
            venue_id,
            attribute,
            value,
            provenance: "researched",
            source_ref,
            observed_at,
            expires_at: None,
            workspace_id,
            licence: None,
        },
    )
    .await
}
