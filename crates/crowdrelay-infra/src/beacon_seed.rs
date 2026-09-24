//! Writing a beacon registry sheet into `beacons`.
//!
//! Unlike `beacon_signal::import` — which files machine research
//! deliberately non-promoting (`verified=false`, `accepts_outreach=false`)
//! so nothing gets contacted unverified — this importer trusts the sheet:
//! the registry workbook is the operator's own curated registry, so a `t`
//! under `Verified` or `Accepts_Outreach` is a person's call and lands
//! as-is. An empty flag cell asserts nothing and never erases a flag the
//! sheet stopped carrying.
//!
//! # Identity
//!
//! The table dedupes on two partial unique indexes — email first,
//! destination URL when there is no email. A generic `ON CONFLICT` cannot
//! match either partial index, so the upsert branches on which route the
//! row carries, exactly as the research importer does. A re-read row is a
//! refresh, not a second beacon.
//!
//! # Provenance
//!
//! `metadata.imported_from` is written on insert and preserved on update —
//! where a row first came from is history, not a field the latest file
//! owns. Refresh-visible sheet facts (`sheet_import`, `source_kind`,
//! `sheet_city` when the city could not resolve) merge on top each cycle.

use crowdrelay_domain::beacon_seed::{BeaconSeedReport, SeededBeacon};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// What one sheet's import did, so the cycle report is honest about rows
/// that refreshed rather than inserted and cities that would not resolve.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BeaconImportSummary {
    /// Newly inserted roster rows.
    pub imported: u64,
    /// Rows the unique identity already knew — fields refreshed in place.
    pub refreshed: u64,
    /// Rows whose city text matched no unambiguous `cities` entry. They
    /// still import — a promoter in an uncatalogued town is a real beacon —
    /// as global (NULL-city) rows, with the sheet's own city text kept in
    /// metadata so a later reconciliation can link them.
    pub unresolved_city: u64,
    /// Rows whose own write failed — isolated per row, logged, counted.
    pub failed: u64,
}

/// What one row's upsert did.
struct BeaconOutcome {
    inserted: bool,
    unresolved_city: bool,
}

/// Postgres-backed beacon-sheet importer.
#[derive(Clone)]
pub struct PostgresBeaconSeedRepository {
    pool: PgPool,
}

impl PostgresBeaconSeedRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Imports every parsed beacon of one sheet. Each row is its own
    /// transaction — one bad row must not take the sheet's other beacons
    /// with it, and a half-written beacon is worse than a refused one —
    /// so a row's error is logged and counted, not propagated.
    pub async fn import_sheet(
        &self,
        workspace_id: Uuid,
        source_file: &str,
        report: &BeaconSeedReport,
    ) -> Result<BeaconImportSummary, sqlx::Error> {
        let mut summary = BeaconImportSummary::default();
        for beacon in &report.beacons {
            match self.import_beacon(workspace_id, source_file, beacon).await {
                Ok(outcome) => {
                    if outcome.inserted {
                        summary.imported += 1;
                    } else {
                        summary.refreshed += 1;
                    }
                    if outcome.unresolved_city {
                        summary.unresolved_city += 1;
                    }
                }
                Err(error) => {
                    summary.failed += 1;
                    tracing::warn!(
                        %error,
                        file = %source_file,
                        beacon = %beacon.display_name,
                        "beacon seed row failed to write"
                    );
                }
            }
        }
        Ok(summary)
    }

    /// One beacon: resolve the city the way `venue_seed` resolves it —
    /// slug first, then a name match only when exactly one city answers,
    /// because a name two cities share picks nothing rather than picking
    /// wrong.
    async fn import_beacon(
        &self,
        workspace_id: Uuid,
        source_file: &str,
        beacon: &SeededBeacon,
    ) -> Result<BeaconOutcome, sqlx::Error> {
        let city_text = beacon
            .city
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());
        let (city_id, unresolved) = match city_text {
            None => (None, false),
            Some(text) => match resolve_city(&self.pool, text).await? {
                Some(id) => (Some(id), false),
                None => (None, true),
            },
        };

        let mut metadata = json!({
            "imported_from": {
                "source": "registry_sheet",
                "file": source_file,
            },
            "sheet_import": {
                "file": source_file,
            },
        });
        if beacon.raw_kind != beacon.kind.as_str()
            && let Some(m) = metadata.as_object_mut()
        {
            m.insert("source_kind".to_owned(), json!(beacon.raw_kind));
        }
        // The sheet's own words, kept so a later city addition or a
        // manual relink can find the row again.
        if unresolved && let Some(m) = metadata.as_object_mut() {
            m.insert("sheet_city".to_owned(), json!(city_text));
        }

        let mut tx = self.pool.begin().await?;
        // `city_id` sits inside both unique identities, so a blind INSERT
        // mints a second beacon whenever the sheet's city resolves
        // differently than the stored row's — the NULL → resolved case
        // above all, where the row first filed global is the same beacon
        // the sheet now places. Find what the row updates BEFORE writing:
        // prefer the exact-identity row, then any candidate at the
        // asserted city, then — when the key names exactly one row —
        // adopt it and let the update follow the sheet. A sheet silent on
        // city never erases one (`COALESCE`), matching every other field.
        let existing: Vec<(Uuid, Option<Uuid>, Option<String>)> = sqlx::query_as(
            "SELECT id, city_id, contact_email FROM beacons \
             WHERE workspace_id = $1 AND beacon_kind = $2 \
               AND (contact_email = $3 OR destination_url = $4) \
             ORDER BY (contact_email IS NOT NULL) DESC",
        )
        .bind(workspace_id)
        .bind(beacon.kind.as_str())
        .bind(beacon.email.as_deref())
        .bind(beacon.destination_url.as_deref())
        .fetch_all(&mut *tx)
        .await?;
        let adopted = existing
            .iter()
            .find(|(_, city, email)| {
                *city == city_id && email.as_deref() == beacon.email.as_deref()
            })
            .or_else(|| existing.iter().find(|(_, city, _)| *city == city_id))
            .map(|(id, _, _)| *id)
            .or(match existing.as_slice() {
                [(only_id, ..)] => Some(*only_id),
                _ => None,
            });
        if let Some(id) = adopted {
            sqlx::query(
                r#"
                UPDATE beacons SET
                    city_id = COALESCE($2, beacons.city_id),
                    contact_email = COALESCE($3, beacons.contact_email),
                    display_name = $4,
                    destination_url = COALESCE($5, beacons.destination_url),
                    source_url = COALESCE($6, beacons.source_url),
                    active = COALESCE($7, beacons.active),
                    verified = COALESCE($8, beacons.verified),
                    accepts_outreach = COALESCE($9, beacons.accepts_outreach),
                    do_not_contact = COALESCE($10, beacons.do_not_contact),
                    relationship_score = COALESCE($11, beacons.relationship_score),
                    relevance_basis_points = COALESCE($12, beacons.relevance_basis_points),
                    confidence_basis_points = COALESCE($13, beacons.confidence_basis_points),
                    metadata = beacons.metadata || ($14::jsonb - 'imported_from'),
                    version = beacons.version + 1
                WHERE id = $1
                "#,
            )
            .bind(id)
            .bind(city_id)
            .bind(beacon.email.as_deref())
            .bind(&beacon.display_name)
            .bind(beacon.destination_url.as_deref())
            .bind(beacon.source_url.as_deref())
            .bind(beacon.active)
            .bind(beacon.verified)
            .bind(beacon.accepts_outreach)
            .bind(beacon.do_not_contact)
            .bind(beacon.relationship_score)
            .bind(beacon.relevance_basis_points)
            .bind(beacon.confidence_basis_points)
            .bind(&metadata)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(BeaconOutcome {
                inserted: false,
                unresolved_city: unresolved,
            });
        }
        // `xmax = 0` tells an insert from a conflict-refresh — the same row
        // comes back from RETURNING either way, so the tag is the only way
        // the summary can say "new" rather than "seen again". The ON
        // CONFLICT arm stays as the race backstop: a concurrent import can
        // land between the candidate read and this write.
        let inserted: bool = if beacon.email.is_some() {
            sqlx::query_scalar::<_, bool>(
                r#"
                INSERT INTO beacons (
                  id, workspace_id, beacon_kind, city_id, display_name,
                  contact_email, destination_url, source_url,
                  active, verified, accepts_outreach, do_not_contact,
                  relationship_score, relevance_basis_points,
                  confidence_basis_points, metadata
                ) VALUES (
                  $1, $2, $3, $4, $5,
                  $6, $7, $8,
                  COALESCE($9, true), COALESCE($10, false),
                  COALESCE($11, false), COALESCE($12, false),
                  COALESCE($13, 50), COALESCE($14, 5000),
                  COALESCE($15, 5000), $16
                )
                ON CONFLICT (workspace_id, beacon_kind, city_id, contact_email)
                  WHERE contact_email IS NOT NULL
                  DO UPDATE SET
                    display_name = EXCLUDED.display_name,
                    destination_url = COALESCE(EXCLUDED.destination_url, beacons.destination_url),
                    source_url = COALESCE(EXCLUDED.source_url, beacons.source_url),
                    active = COALESCE(EXCLUDED.active, beacons.active),
                    verified = COALESCE(EXCLUDED.verified, beacons.verified),
                    accepts_outreach = COALESCE(EXCLUDED.accepts_outreach, beacons.accepts_outreach),
                    do_not_contact = COALESCE(EXCLUDED.do_not_contact, beacons.do_not_contact),
                    relationship_score = COALESCE(EXCLUDED.relationship_score, beacons.relationship_score),
                    relevance_basis_points = COALESCE(EXCLUDED.relevance_basis_points, beacons.relevance_basis_points),
                    confidence_basis_points = COALESCE(EXCLUDED.confidence_basis_points, beacons.confidence_basis_points),
                    metadata = beacons.metadata || (EXCLUDED.metadata - 'imported_from'),
                    version = beacons.version + 1
                RETURNING (xmax = 0)
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id)
            .bind(beacon.kind.as_str())
            .bind(city_id)
            .bind(&beacon.display_name)
            .bind(beacon.email.as_deref())
            .bind(beacon.destination_url.as_deref())
            .bind(beacon.source_url.as_deref())
            .bind(beacon.active)
            .bind(beacon.verified)
            .bind(beacon.accepts_outreach)
            .bind(beacon.do_not_contact)
            .bind(beacon.relationship_score)
            .bind(beacon.relevance_basis_points)
            .bind(beacon.confidence_basis_points)
            .bind(&metadata)
            .fetch_one(&mut *tx)
            .await?
        } else {
            sqlx::query_scalar::<_, bool>(
                r#"
                INSERT INTO beacons (
                  id, workspace_id, beacon_kind, city_id, display_name,
                  contact_email, destination_url, source_url,
                  active, verified, accepts_outreach, do_not_contact,
                  relationship_score, relevance_basis_points,
                  confidence_basis_points, metadata
                ) VALUES (
                  $1, $2, $3, $4, $5,
                  NULL, $6, $7,
                  COALESCE($8, true), COALESCE($9, false),
                  COALESCE($10, false), COALESCE($11, false),
                  COALESCE($12, 50), COALESCE($13, 5000),
                  COALESCE($14, 5000), $15
                )
                ON CONFLICT (workspace_id, beacon_kind, city_id, destination_url)
                  WHERE contact_email IS NULL AND destination_url IS NOT NULL
                  DO UPDATE SET
                    display_name = EXCLUDED.display_name,
                    source_url = COALESCE(EXCLUDED.source_url, beacons.source_url),
                    active = COALESCE(EXCLUDED.active, beacons.active),
                    verified = COALESCE(EXCLUDED.verified, beacons.verified),
                    accepts_outreach = COALESCE(EXCLUDED.accepts_outreach, beacons.accepts_outreach),
                    do_not_contact = COALESCE(EXCLUDED.do_not_contact, beacons.do_not_contact),
                    relationship_score = COALESCE(EXCLUDED.relationship_score, beacons.relationship_score),
                    relevance_basis_points = COALESCE(EXCLUDED.relevance_basis_points, beacons.relevance_basis_points),
                    confidence_basis_points = COALESCE(EXCLUDED.confidence_basis_points, beacons.confidence_basis_points),
                    metadata = beacons.metadata || (EXCLUDED.metadata - 'imported_from'),
                    version = beacons.version + 1
                RETURNING (xmax = 0)
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id)
            .bind(beacon.kind.as_str())
            .bind(city_id)
            .bind(&beacon.display_name)
            .bind(beacon.destination_url.as_deref())
            .bind(beacon.source_url.as_deref())
            .bind(beacon.active)
            .bind(beacon.verified)
            .bind(beacon.accepts_outreach)
            .bind(beacon.do_not_contact)
            .bind(beacon.relationship_score)
            .bind(beacon.relevance_basis_points)
            .bind(beacon.confidence_basis_points)
            .bind(&metadata)
            .fetch_one(&mut *tx)
            .await?
        };
        tx.commit().await?;
        Ok(BeaconOutcome {
            inserted,
            unresolved_city: unresolved,
        })
    }
}

/// The venue resolver's rule with its ambiguity hole closed: `cities.slug`
/// is unique per *country*, not globally, so matching on slug alone can
/// silently pick another country's city. Here both the slug and the
/// lower-cased name must name exactly one row — a shared slug, a shared
/// name, or a slug that is another city's name all resolve to nothing
/// rather than to a guess.
async fn resolve_city(pool: &PgPool, city_text: &str) -> Result<Option<Uuid>, sqlx::Error> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities \
         WHERE slug = lower(btrim($1)) OR lower(btrim(name)) = lower(btrim($1)) \
         LIMIT 2",
    )
    .bind(city_text)
    .fetch_all(pool)
    .await?;
    Ok(match ids.as_slice() {
        [only] => Some(*only),
        _ => None,
    })
}
