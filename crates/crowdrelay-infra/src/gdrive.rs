//! Google Drive contacts staging — `drive_contacts`.
//!
//! Every email the connector extracts lands here first, deduplicated by
//! `(workspace_id, normalized_email)`. Nothing reaches `fans` or
//! `agent_outreach_targets` without an explicit promote — and a promote
//! routes through the same write paths every other import uses, so consent
//! (pending + double-opt-in) and screening (`proposed` targets) are never
//! bypassed.
//!
//! `fan_outcome` and `beacon_outcome` are independent on purpose: a beacon
//! may also be a fan.

use crowdrelay_domain::drive_contacts::ExtractedContact;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum GDriveError {
    #[error("gdrive database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("drive contact not found")]
    NotFound,
    #[error("not a booking target kind")]
    InvalidKind,
}

/// What a booking-kind promote did. The non-`Done` arms are not errors: the
/// contact stays `staged` and the caller tells the operator what is missing.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BookingPromoteOutcome {
    Done,
    /// No city was given and the venue registry could not place the name.
    CityRequired,
    /// A city was given that is not in the catalogue — a typo the operator
    /// can fix, not a state to file.
    UnknownCity,
    /// A candidate for the same route was already refused; the promote did
    /// not overturn that decision.
    RouteRefused,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DriveContactRow {
    pub id: Uuid,
    pub normalized_email: String,
    pub display_name: Option<String>,
    pub organization: Option<String>,
    pub phone: Option<String>,
    pub suggested_kind: Option<String>,
    /// The city the sheet placed this contact in — free text, resolved
    /// against `cities` only when a booking promote needs it.
    pub city: Option<String>,
    pub notes: Option<String>,
    pub source_file_id: String,
    pub source_file_name: String,
    pub sources: Vec<String>,
    pub last_seen_at: time::OffsetDateTime,
    pub disappeared_at: Option<time::OffsetDateTime>,
    pub fan_outcome: String,
    pub beacon_outcome: String,
    /// P.2 registry join: the organisation (or the name itself, for a venue
    /// typed row) resolved against `place_venues`. Some(display_name) means
    /// this room is already on record somewhere — maybe played by this
    /// band, maybe known only through another tenant's marks.
    pub matched_venue: Option<String>,
    /// The matched room's registry id — the key the cross-tenant prior
    /// lookup joins on. Internal: the API nests the prior, not the id.
    pub matched_venue_id: Option<Uuid>,
    /// This workspace's own marks say the band already played that room.
    pub venue_played_here: bool,
    /// `place_counterparties` knows this address — a promoter or booker an
    /// event already recorded, in this or another tenant's ledger.
    pub matched_counterparty: Option<String>,
    /// This workspace's own marks say the band already dealt with them.
    pub counterparty_worked_with: bool,
}

/// A staged contact plus the cross-tenant priors its addresses and rooms
/// carry (P.6). The row is the tenant's own; the priors are the shared
/// record — anonymous counts, the only shape cross-tenant history is
/// allowed to leave this crate in.
pub struct DriveContactView {
    pub row: DriveContactRow,
    /// The address's reply record across every tenant. `Some` when the
    /// prior read ran — `None` means "not measured", never "no history".
    pub counterparty_prior: Option<crate::cross_tenant_priors::CounterpartyPrior>,
    /// The matched room's play record across every tenant. `None` when the
    /// row matched no `place_venues` row or the read did not run.
    pub venue_prior: Option<crate::cross_tenant_priors::VenuePrior>,
}

/// What the registry join found across the whole review population, not
/// just the returned page — the "412 already in the venue registry" half of
/// the sheet arriving.
#[derive(Debug, Default, Clone, Copy, sqlx::FromRow)]
pub struct RegistrySummary {
    pub total: i64,
    pub known_venues: i64,
    pub own_rooms: i64,
    pub known_counterparties: i64,
    pub dealt_with: i64,
}

/// The registry-resolved contact row shape. Every reader of
/// `drive_contacts` selects through this so `DriveContactRow`
/// decodes identically whether it came from the review queue or a single
/// fetch — a second column list is a decode fault waiting on the first
/// divergent field.
///
/// A venue matches when the organisation (or the row's own name, for a
/// venue-shaped contact) keys to a `place_venues` row that either sits in
/// the city the sheet named or is the only room anywhere carrying that
/// name — the same unambiguous-or-placed discipline the booking promote
/// applies. A counterparty matches on the address alone: the registry's
/// identity key is the email. `ORDER BY played_here DESC` inside the
/// lateral makes a same-named room the band already played win the tie.
const CONTACT_SELECT: &str = r#"
    SELECT c.id, c.normalized_email, c.display_name, c.organization, c.phone,
           c.suggested_kind, c.city, c.notes, c.source_file_id, c.source_file_name,
           c.sources, c.last_seen_at, c.disappeared_at, c.fan_outcome, c.beacon_outcome,
           venue.display_name AS matched_venue,
           venue.venue_id AS matched_venue_id,
           COALESCE(venue.played_here, false) AS venue_played_here,
           cp.display_name AS matched_counterparty,
           COALESCE(cp.dealt_with, false) AS counterparty_worked_with
    FROM drive_contacts c
    LEFT JOIN LATERAL (
        SELECT v.id AS venue_id, v.display_name,
               EXISTS (
                   SELECT 1 FROM place_venue_marks vm
                   WHERE vm.venue_id = v.id
                     AND vm.workspace_id = c.workspace_id
               ) AS played_here
        FROM place_venues v
        JOIN cities vc ON vc.id = v.city_id
        WHERE v.name_key = place_venue_key(
                  COALESCE(c.organization, c.display_name, ''))
          AND (
              vc.slug = lower(btrim(COALESCE(c.city, '')))
              OR lower(vc.name) = lower(btrim(COALESCE(c.city, '')))
              OR (SELECT count(*) FROM place_venues sib
                  WHERE sib.name_key = v.name_key) = 1
          )
        ORDER BY played_here DESC
        LIMIT 1
    ) venue ON true
    LEFT JOIN LATERAL (
        SELECT p.display_name,
               EXISTS (
                   SELECT 1 FROM place_counterparty_marks cm
                   WHERE cm.counterparty_id = p.id
                     AND cm.workspace_id = c.workspace_id
               ) AS dealt_with
        FROM place_counterparties p
        WHERE p.email_key = c.normalized_email
        LIMIT 1
    ) cp ON true
"#;

/// What one file's upsert did.
#[derive(Debug, Default, Clone, Copy)]
pub struct ContactUpsertSummary {
    pub upserted: u64,
    pub marked_disappeared: u64,
}

#[derive(Clone)]
pub struct PostgresGDriveRepository {
    pool: PgPool,
}

impl PostgresGDriveRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool, for the worker's `PgListener` — same connection family
    /// the repository writes through.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The recorded Drive modifiedTime for a file, if scanned before.
    /// Matching mtime means the export is byte-identical to last cycle's
    /// and the whole file can be skipped.
    pub async fn file_mtime(
        &self,
        workspace_id: Uuid,
        file_id: &str,
    ) -> Result<Option<String>, GDriveError> {
        sqlx::query_scalar::<_, String>(
            "SELECT last_mtime FROM drive_files WHERE workspace_id = $1 AND file_id = $2",
        )
        .bind(workspace_id)
        .bind(file_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(GDriveError::Database)
    }

    /// Upserts per-file scan state. `no_email_column` means the file is a
    /// spreadsheet but not a contact list — the UI can say that instead of
    /// the file being invisible.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_file_state(
        &self,
        workspace_id: Uuid,
        file_id: &str,
        file_name: &str,
        mime_type: &str,
        mtime: &str,
        no_email_column: bool,
        rows_read: i32,
        contacts_written: i32,
    ) -> Result<(), GDriveError> {
        // file_name is capped at 500 by CHECK — a longer Drive name must
        // truncate, not fail the file's state write.
        let file_name: String = file_name.chars().take(500).collect();
        sqlx::query(
            r#"
            INSERT INTO drive_files
                (workspace_id, file_id, file_name, mime_type, last_mtime,
                 no_email_column, rows_read, contacts_written, last_scanned_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
            ON CONFLICT (workspace_id, file_id) DO UPDATE SET
                file_name = EXCLUDED.file_name,
                mime_type = EXCLUDED.mime_type,
                last_mtime = EXCLUDED.last_mtime,
                no_email_column = EXCLUDED.no_email_column,
                rows_read = EXCLUDED.rows_read,
                contacts_written = EXCLUDED.contacts_written,
                last_scanned_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(file_id)
        .bind(&file_name)
        .bind(mime_type)
        .bind(mtime)
        .bind(no_email_column)
        .bind(rows_read)
        .bind(contacts_written)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The Google account ref a connection's tokens were encrypted under —
    /// the AAD component point-of-use decrypts need.
    pub async fn connection_account_ref(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
    ) -> Result<String, GDriveError> {
        sqlx::query_scalar::<_, String>(
            "SELECT external_account_ref FROM fanbase_connections WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(connection_id)
        .fetch_one(&self.pool)
        .await
        .map_err(GDriveError::Database)
    }

    /// Upserts one source-item's extracted contacts in a single
    /// transaction. `source` is the intake origin (`gdrive` | `gmail`);
    /// `ref_id`/`ref_name` are provenance (a Drive file id/name or a Gmail
    /// message id/subject line).
    ///
    /// An existing address gets its fields refreshed and its `sources`
    /// array unioned — a contact found in both Drive and Gmail is one row
    /// with two sources. `mark_disappeared` is true only for Drive files:
    /// an edited sheet is the truth about that file, whereas mail is never
    /// re-listed after the history cursor passes it.
    pub async fn upsert_contacts_for_source(
        &self,
        workspace_id: Uuid,
        source: &str,
        ref_id: &str,
        ref_name: &str,
        contacts: &[ExtractedContact],
        mark_disappeared: bool,
    ) -> Result<ContactUpsertSummary, GDriveError> {
        let mut tx = self.pool.begin().await?;

        // Deterministic write order: two intakes upserting overlapping
        // addresses in different orders is how a deadlock is born.
        let mut ordered: Vec<&ExtractedContact> = contacts.iter().collect();
        ordered.sort_by(|a, b| a.email.cmp(&b.email));

        // source_file_id is capped at 200 and source_file_name at 500 by
        // CHECK — the upload id embeds a filename the operator controls.
        let ref_id: String = ref_id.chars().take(200).collect();
        let ref_name: String = ref_name.chars().take(500).collect();

        let mut upserted = 0u64;
        for contact in ordered {
            sqlx::query(
                r#"
                INSERT INTO drive_contacts (
                    workspace_id, normalized_email, display_name, organization,
                    phone, suggested_kind, city, notes, source_file_id, source_file_name,
                    sources, last_seen_at, disappeared_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, ARRAY[$11]::text[], now(), NULL)
                ON CONFLICT (workspace_id, normalized_email) DO UPDATE SET
                    display_name = COALESCE(EXCLUDED.display_name, drive_contacts.display_name),
                    organization = COALESCE(EXCLUDED.organization, drive_contacts.organization),
                    phone = COALESCE(EXCLUDED.phone, drive_contacts.phone),
                    suggested_kind = COALESCE(EXCLUDED.suggested_kind, drive_contacts.suggested_kind),
                    city = COALESCE(EXCLUDED.city, drive_contacts.city),
                    notes = COALESCE(EXCLUDED.notes, drive_contacts.notes),
                    -- source_file_id doubles as the disappearance anchor:
                    -- the mark_disappeared sweep matches rows by the file
                    -- that last saw them. A sighting from a source that
                    -- never marks (gmail, upload) must not steal the anchor
                    -- from a gdrive file that still tracks it.
                    source_file_id = CASE
                        WHEN $11 = 'gdrive'
                             OR NOT 'gdrive' = ANY(drive_contacts.sources)
                        THEN EXCLUDED.source_file_id
                        ELSE drive_contacts.source_file_id
                    END,
                    source_file_name = CASE
                        WHEN $11 = 'gdrive'
                             OR NOT 'gdrive' = ANY(drive_contacts.sources)
                        THEN EXCLUDED.source_file_name
                        ELSE drive_contacts.source_file_name
                    END,
                    sources = (SELECT array_agg(DISTINCT s) FROM unnest(
                        drive_contacts.sources || EXCLUDED.sources) AS s),
                    last_seen_at = now(),
                    disappeared_at = NULL
                "#,
            )
            .bind(workspace_id)
            .bind(&contact.email)
            .bind(&contact.display_name)
            .bind(&contact.organization)
            .bind(&contact.phone)
            .bind(&contact.suggested_kind)
            .bind(&contact.city)
            .bind(&contact.notes)
            .bind(&ref_id)
            .bind(&ref_name)
            .bind(source)
            .execute(&mut *tx)
            .await?;
            upserted += 1;
        }

        let mut marked = 0u64;
        if mark_disappeared {
            // Addresses this file used to carry that it no longer does. Only
            // staged rows — a promoted or dismissed contact keeps its
            // outcome.
            marked = sqlx::query(
                r#"
            UPDATE drive_contacts
            SET disappeared_at = now()
            WHERE workspace_id = $1
              AND source_file_id = $2
              AND fan_outcome = 'staged'
              AND beacon_outcome = 'staged'
              AND disappeared_at IS NULL
              AND normalized_email <> ALL ($3::text[])
            "#,
            )
            .bind(workspace_id)
            .bind(&ref_id)
            .bind(
                contacts
                    .iter()
                    .map(|c| c.email.as_str())
                    .collect::<Vec<_>>(),
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }

        tx.commit().await?;
        Ok(ContactUpsertSummary {
            upserted,
            marked_disappeared: marked,
        })
    }

    /// The moment this address last wrote to the band's mailbox — message
    /// `From` = the address, `internalDate` of the message. `last_seen_at`
    /// is any sighting in any header; this is direction. Monotonic: an
    /// earlier timestamp never lowers the stored one.
    pub async fn record_inbound_sighting(
        &self,
        workspace_id: Uuid,
        normalized_email: &str,
        at: time::OffsetDateTime,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            r#"
            UPDATE drive_contacts
            SET last_inbound_at = GREATEST(COALESCE(last_inbound_at, $3), $3),
                updated_at = now()
            WHERE workspace_id = $1 AND normalized_email = $2
            "#,
        )
        .bind(workspace_id)
        .bind(normalized_email)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The review queue: staged rows first, newest seen first, every row
    /// resolved against the shared registries so a sheet arrives as "412
    /// already on record" rather than 2000 strangers (P.2).
    pub async fn list_contacts(
        &self,
        workspace_id: Uuid,
        limit: i64,
    ) -> Result<Vec<DriveContactView>, GDriveError> {
        let sql = format!(
            r#"{CONTACT_SELECT}
            WHERE c.workspace_id = $1
            ORDER BY (c.fan_outcome = 'staged' OR c.beacon_outcome = 'staged') DESC,
                     c.last_seen_at DESC
            LIMIT $2"#
        );
        let rows = sqlx::query_as::<_, DriveContactRow>(&sql)
            .bind(workspace_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(GDriveError::Database)?;
        Ok(self.attach_priors(rows).await)
    }

    /// The P.6 layer: every row's address asks the shared ledger for its
    /// reply record, and every matched room asks the marks. A prior read
    /// that fails leaves the row whole — the contacts are the payload, the
    /// priors are the annotation, so an absent one serializes as `null`,
    /// not as a measured zero.
    async fn attach_priors(&self, rows: Vec<DriveContactRow>) -> Vec<DriveContactView> {
        let emails: Vec<String> = rows
            .iter()
            .map(|row| row.normalized_email.clone())
            .collect();
        let venue_ids: Vec<Uuid> = rows.iter().filter_map(|row| row.matched_venue_id).collect();
        let counterparty_priors =
            crate::cross_tenant_priors::counterparty_priors(&self.pool, &emails)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "drive contacts: counterparty prior read failed");
                    error
                })
                .ok();
        let venue_priors = crate::cross_tenant_priors::venue_priors(&self.pool, &venue_ids)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "drive contacts: venue prior read failed");
                error
            })
            .ok();
        rows.into_iter()
            .map(|row| DriveContactView {
                counterparty_prior: counterparty_priors.as_ref().map(|priors| {
                    priors
                        .get(&row.normalized_email)
                        .copied()
                        .unwrap_or_default()
                }),
                venue_prior: row
                    .matched_venue_id
                    .and_then(|venue_id| {
                        venue_priors
                            .as_ref()
                            .and_then(|priors| priors.get(&venue_id).copied())
                    })
                    .or_else(|| {
                        // A matched room with no marks anywhere still has a
                        // measured record: nobody on record played it.
                        if row.matched_venue_id.is_some() && venue_priors.is_some() {
                            Some(crate::cross_tenant_priors::VenuePrior::default())
                        } else {
                            None
                        }
                    }),
                row,
            })
            .collect()
    }

    /// The same joins, counted across every row — including ones the
    /// paginated list never returned — so the summary cannot claim a match
    /// rate measured on a page.
    pub async fn registry_summary(
        &self,
        workspace_id: Uuid,
    ) -> Result<RegistrySummary, GDriveError> {
        sqlx::query_as::<_, RegistrySummary>(
            r#"
            SELECT count(*) AS total,
                   count(*) FILTER (WHERE venue.display_name IS NOT NULL) AS known_venues,
                   count(*) FILTER (WHERE venue.played_here) AS own_rooms,
                   count(*) FILTER (WHERE cp.counterparty_id IS NOT NULL) AS known_counterparties,
                   count(*) FILTER (WHERE cp.dealt_with) AS dealt_with
            FROM drive_contacts c
            LEFT JOIN LATERAL (
                SELECT v.display_name,
                       EXISTS (
                           SELECT 1 FROM place_venue_marks vm
                           WHERE vm.venue_id = v.id
                             AND vm.workspace_id = c.workspace_id
                       ) AS played_here
                FROM place_venues v
                JOIN cities vc ON vc.id = v.city_id
                WHERE v.name_key = place_venue_key(
                          COALESCE(c.organization, c.display_name, ''))
                  AND (
                      vc.slug = lower(btrim(COALESCE(c.city, '')))
                      OR lower(vc.name) = lower(btrim(COALESCE(c.city, '')))
                      OR (SELECT count(*) FROM place_venues sib
                          WHERE sib.name_key = v.name_key) = 1
                  )
                ORDER BY played_here DESC
                LIMIT 1
            ) venue ON true
            LEFT JOIN LATERAL (
                SELECT p.id AS counterparty_id,
                       EXISTS (
                           SELECT 1 FROM place_counterparty_marks cm
                           WHERE cm.counterparty_id = p.id
                             AND cm.workspace_id = c.workspace_id
                       ) AS dealt_with
                FROM place_counterparties p
                WHERE p.email_key = c.normalized_email
                LIMIT 1
            ) cp ON true
            WHERE c.workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await
        .map_err(GDriveError::Database)
    }

    /// Sets one destination's outcome. Returns the row so the caller can
    /// render the post-write state; NotFound when the id is not this
    /// workspace's.
    pub async fn set_outcome(
        &self,
        workspace_id: Uuid,
        contact_id: Uuid,
        destination: &str,
        outcome: &str,
    ) -> Result<DriveContactRow, GDriveError> {
        let column = match (destination, outcome) {
            ("fan", "promoted" | "dismissed") => "fan_outcome",
            ("beacon", "promoted" | "dismissed") => "beacon_outcome",
            _ => return Err(GDriveError::NotFound),
        };
        let updated = sqlx::query_scalar::<_, Uuid>(&format!(
            "UPDATE drive_contacts SET {column} = $3 \
                 WHERE workspace_id = $1 AND id = $2 RETURNING id"
        ))
        .bind(workspace_id)
        .bind(contact_id)
        .bind(outcome)
        .fetch_optional(&self.pool)
        .await?;
        match updated {
            Some(_) => self.get_contact(workspace_id, contact_id).await,
            None => Err(GDriveError::NotFound),
        }
    }

    /// One staged contact by id, for the promote handlers — the same
    /// registry-resolved shape the review queue returns.
    pub async fn get_contact(
        &self,
        workspace_id: Uuid,
        contact_id: Uuid,
    ) -> Result<DriveContactRow, GDriveError> {
        sqlx::query_as::<_, DriveContactRow>(&format!(
            "{CONTACT_SELECT} WHERE c.workspace_id = $1 AND c.id = $2"
        ))
        .bind(workspace_id)
        .bind(contact_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(GDriveError::NotFound)
    }

    /// The catalogue city a staged contact's own city column names, if it
    /// names one unambiguously.
    ///
    /// Slug or display name — "Wrocław" and "wroclaw" land on the same row.
    /// A name two catalogued cities share resolves to nothing, exactly as the
    /// booking half treats it: guessing files a contact against the wrong
    /// place, and a press contact filed in the wrong city is worse than one
    /// filed in none, because the wrong city's show will suggest it.
    async fn staged_city_id(&self, city: Option<&str>) -> Result<Option<Uuid>, GDriveError> {
        let Some(city) = city.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        let matches = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM cities \
             WHERE slug = lower(btrim($1)) OR lower(btrim(name)) = lower(btrim($1))",
        )
        .bind(city)
        .fetch_all(&self.pool)
        .await?;
        Ok(match matches.as_slice() {
            [only] => Some(*only),
            _ => None,
        })
    }

    /// Promotes a staged contact to the outreach pipeline: a `proposed`
    /// `agent_outreach_targets` row — the same status and the same conflict
    /// key the curated-CRM import uses, so screening and operator approval
    /// still decide what the growth loop may act on. `dismissed` targets
    /// are never revived.
    pub async fn promote_beacon(
        &self,
        workspace_id: Uuid,
        contact: &DriveContactRow,
        target_kind: &str,
    ) -> Result<(), GDriveError> {
        // §4h-11 / 3.9: a press contact keeps the city the sheet placed it in,
        // when the catalogue recognises one. Unlike the booking half, a missing
        // city is not a refusal — a national magazine is somewhere in the sense
        // that matters to nobody, and demanding one would either lose the
        // contact or file it under a city it does not belong to.
        let city_id = self.staged_city_id(contact.city.as_deref()).await?;
        let mut tx = self.pool.begin().await?;
        // display_name is capped at 200 by the outreach CHECK — an email
        // fallback can reach 254, so truncate rather than fail the promote.
        let display_name: String = contact
            .display_name
            .clone()
            .or_else(|| contact.organization.clone())
            .unwrap_or_else(|| contact.normalized_email.clone())
            .trim()
            .chars()
            .take(200)
            .collect();
        // A whitespace-only display name would fail `btrim <> ''` at write.
        let display_name = if display_name.is_empty() {
            contact
                .organization
                .clone()
                .unwrap_or_else(|| contact.normalized_email.clone())
                .trim()
                .chars()
                .take(200)
                .collect()
        } else {
            display_name
        };
        let domain = contact
            .normalized_email
            .rsplit_once('@')
            .map(|(_, domain)| domain.to_owned());
        let why_fit = {
            let mut parts = Vec::new();
            if let Some(notes) = &contact.notes {
                parts.push(notes.clone());
            }
            if let Some(org) = &contact.organization {
                parts.push(format!("org={org}"));
            }
            parts.join(" · ")
        };
        sqlx::query(
            r#"
            INSERT INTO agent_outreach_targets
                (workspace_id, target_kind, display_name, contact_email,
                 contact_domain, why_fit, evidence, status, city_id)
            VALUES ($1,$2,$3,$4,$5,$6,$7,'proposed',$8)
            ON CONFLICT (workspace_id, display_name, target_kind) DO UPDATE SET
                contact_email = COALESCE(EXCLUDED.contact_email, agent_outreach_targets.contact_email),
                contact_domain = COALESCE(EXCLUDED.contact_domain, agent_outreach_targets.contact_domain),
                -- A city already on file wins: it was resolved once and may
                -- have been corrected by hand since. A re-import that knows
                -- less must not erase what the operator knows.
                city_id = COALESCE(agent_outreach_targets.city_id, EXCLUDED.city_id),
                why_fit = COALESCE(NULLIF(EXCLUDED.why_fit, ''), agent_outreach_targets.why_fit),
                evidence = CASE
                    WHEN jsonb_array_length(EXCLUDED.evidence) > 0 THEN EXCLUDED.evidence
                    ELSE agent_outreach_targets.evidence
                END
            "#,
        )
        .bind(workspace_id)
        .bind(target_kind)
        .bind(&display_name)
        .bind(&contact.normalized_email)
        .bind(domain)
        .bind(why_fit)
        .bind(serde_json::json!([{
            "source": contact.sources.join("+"),
            "file": contact.source_file_name,
        }]))
        .bind(city_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE drive_contacts SET beacon_outcome = 'promoted' WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(contact.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Promotes a contact onto the representation list — an agent or label
    /// the band's own files already knew. First-party provenance is why
    /// `verified` lands true: the address came out of the band's drive, not
    /// a researched guess. Consent is *not* presumed — `accepts_outreach`
    /// stays false and the basis stays empty until the band says why this
    /// contact accepts approaches, which the schema CHECK requires anyway.
    ///
    /// One address is one target (`UNIQUE (workspace_id, contact_email)`):
    /// if the email is already filed under another kind the kind is left
    /// alone — a press writer who also books acts keeps their existing
    /// row, and the contact still counts as filed.
    pub async fn promote_beacon_representation(
        &self,
        workspace_id: Uuid,
        contact: &DriveContactRow,
        target_kind: &str,
    ) -> Result<(), GDriveError> {
        if !matches!(target_kind, "agent" | "label") {
            return Err(GDriveError::InvalidKind);
        }
        let mut tx = self.pool.begin().await?;
        let display_name: String = contact
            .display_name
            .clone()
            .or_else(|| contact.organization.clone())
            .unwrap_or_else(|| contact.normalized_email.clone())
            .trim()
            .chars()
            .take(200)
            .collect();
        let display_name = if display_name.is_empty() {
            contact.normalized_email.trim().chars().take(200).collect()
        } else {
            display_name
        };
        upsert_representation_target(
            &mut tx,
            workspace_id,
            target_kind,
            &display_name,
            &contact.normalized_email,
        )
        .await?;
        sqlx::query(
            "UPDATE drive_contacts SET beacon_outcome = 'promoted' WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(contact.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Promotes a contact onto the booking-agent list — a
    /// `booking_agents` row (§12-5 entity 4). An agent is the one
    /// booking-graph contact that is *not* city-scoped: the band pitches the
    /// agent for representation once, not a room for one night, so the
    /// candidate queue's city dance never applies. The sheet's organization
    /// column carries the agency — the firm the agent works for is half of
    /// who they are.
    ///
    /// One address is one agent (`UNIQUE (workspace_id, contact_email)`): a
    /// re-import refreshes name and agency rather than filing a twin, and a
    /// refusal already on file (`refused_until`) is never touched — the
    /// closed door is the agent's answer, not the sheet's to reopen.
    pub async fn promote_beacon_agent(
        &self,
        workspace_id: Uuid,
        contact: &DriveContactRow,
    ) -> Result<(), GDriveError> {
        let mut tx = self.pool.begin().await?;
        // Same name-fallback chain as the other promotes — the sheet's
        // display name, else the organization, else the address itself.
        let name: String = contact
            .display_name
            .clone()
            .or_else(|| contact.organization.clone())
            .unwrap_or_else(|| contact.normalized_email.clone())
            .trim()
            .chars()
            .take(200)
            .collect();
        let name = if name.is_empty() {
            contact.normalized_email.trim().chars().take(200).collect()
        } else {
            name
        };
        // The agency and the name share a column source — when the display
        // name fell back to the organization, filing the same string twice
        // reads as a data error, so agency yields.
        let agency = contact
            .organization
            .clone()
            .filter(|org| name != org.trim())
            .map(|org| org.trim().chars().take(200).collect::<String>())
            .filter(|org| !org.is_empty());
        sqlx::query(
            r#"
            INSERT INTO booking_agents
                (workspace_id, name, agency, contact_email, contact_verified_at)
            VALUES ($1, $2, $3, $4, now())
            ON CONFLICT (workspace_id, contact_email) DO UPDATE SET
                name = EXCLUDED.name,
                agency = COALESCE(EXCLUDED.agency, booking_agents.agency),
                contact_verified_at = COALESCE(
                    booking_agents.contact_verified_at, now()
                )
            "#,
        )
        .bind(workspace_id)
        .bind(&name)
        .bind(agency)
        .bind(&contact.normalized_email)
        .execute(&mut *tx)
        .await?;
        // The registry row is who the agent is; the outreach row is how the
        // band reaches them — one promote files both, or the agent list
        // would be a book the band can never act on. Consent still waits on
        // a stated basis like every representation contact.
        upsert_representation_target(
            &mut tx,
            workspace_id,
            "agent",
            &name,
            &contact.normalized_email,
        )
        .await?;
        sqlx::query(
            "UPDATE drive_contacts SET beacon_outcome = 'promoted' WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(contact.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Promotes a contact into booking supply: a `booking_candidates`
    /// row, admitted on first-party grounds — the band's own sent mail is the
    /// evidence (`source_reference` carries the thread), which is exactly what
    /// `screen_candidate`'s inferred-route refusal exists to distinguish from.
    /// A candidate is still not a target: `confirm_booking_candidate` and its
    /// city requirement still stand between this row and outreach.
    ///
    /// Booking targets are city-scoped, so the candidate needs one: the
    /// explicit `city_slug` wins; absent that, an unambiguous `place_venues`
    /// name match resolves the room the band already played. Anything else
    /// refuses with [`BookingPromoteOutcome::CityRequired`] — a candidate with
    /// no city can never promote and nothing would fix it later.
    pub async fn promote_beacon_booking(
        &self,
        workspace_id: Uuid,
        contact: &DriveContactRow,
        target_kind: &str,
        city_slug: Option<&str>,
    ) -> Result<BookingPromoteOutcome, GDriveError> {
        if !matches!(target_kind, "venue" | "promoter" | "festival") {
            return Err(GDriveError::InvalidKind);
        }
        // Whitespace-only display names are storable in staging but dead in
        // candidates (`btrim <> ''`) — fall through to org/email instead of
        // failing the write.
        let display_name: String = contact
            .display_name
            .clone()
            .or_else(|| contact.organization.clone())
            .unwrap_or_else(|| contact.normalized_email.clone())
            .trim()
            .chars()
            .take(200)
            .collect();
        let display_name = if display_name.is_empty() {
            contact
                .organization
                .clone()
                .unwrap_or_else(|| contact.normalized_email.clone())
                .trim()
                .chars()
                .take(200)
                .collect()
        } else {
            display_name
        };
        let city_slug = match city_slug.map(str::trim).filter(|c| !c.is_empty()) {
            // An explicit slug must name a real city — a typo filed here is
            // an admitted candidate that can never confirm and can never be
            // refiled (route-identity conflict), so the check happens now.
            Some(slug) => match sqlx::query_scalar::<_, String>(
                "SELECT slug FROM cities WHERE slug = lower(btrim($1))",
            )
            .bind(slug)
            .fetch_optional(&self.pool)
            .await?
            {
                Some(resolved) => Some(resolved),
                None => return Ok(BookingPromoteOutcome::UnknownCity),
            },
            None => {
                // The sheet's own city column resolves next, by slug or by
                // an unambiguous name — "Wrocław" and "wroclaw" land on the
                // same slug. A name two catalogued cities share picks
                // nothing, the way two rooms sharing a name pick nothing:
                // guessing files the candidate against the wrong place.
                let staged = contact
                    .city
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty());
                let staged_slug = match staged {
                    Some(city) => {
                        let slugs = sqlx::query_scalar::<_, String>(
                            "SELECT DISTINCT slug FROM cities \
                             WHERE slug = lower(btrim($1)) \
                                OR lower(btrim(name)) = lower(btrim($1))",
                        )
                        .bind(city)
                        .fetch_all(&self.pool)
                        .await?;
                        match slugs.as_slice() {
                            [only] => Some(only.clone()),
                            _ => None,
                        }
                    }
                    None => None,
                };
                match staged_slug {
                    Some(slug) => Some(slug),
                    None => {
                        let matches = sqlx::query_scalar::<_, Uuid>(
                            r#"
                            SELECT DISTINCT v.city_id
                            FROM place_venues v
                            JOIN place_venue_marks m
                              ON m.venue_id = v.id AND m.workspace_id = $1
                            WHERE v.name_key = place_venue_key($2)
                               OR v.name_key = place_venue_key(COALESCE($3, ''))
                            "#,
                        )
                        .bind(workspace_id)
                        .bind(&display_name)
                        .bind(contact.organization.as_deref().unwrap_or_default())
                        .fetch_all(&self.pool)
                        .await?;
                        match matches.as_slice() {
                            [only] => {
                                sqlx::query_scalar::<_, String>(
                                    "SELECT slug FROM cities WHERE id = $1",
                                )
                                .bind(*only)
                                .fetch_optional(&self.pool)
                                .await?
                            }
                            _ => None,
                        }
                    }
                }
            }
        };
        let Some(city_slug) = city_slug else {
            return Ok(BookingPromoteOutcome::CityRequired);
        };
        let mut tx = self.pool.begin().await?;
        let evidence = {
            let mut parts = vec![format!("thread: {}", contact.source_file_name)];
            if let Some(notes) = &contact.notes {
                parts.push(notes.clone());
            }
            if let Some(org) = &contact.organization {
                parts.push(format!("org={org}"));
            }
            let joined = parts.join(" · ");
            joined.chars().take(4000).collect::<String>()
        };
        // fit_basis_points sits at the discovery floor on purpose: a
        // first-party contact is not scored for fit — the band's own thread
        // is the admission — but it must not read *below* the floor to any
        // consumer that applies it to admitted rows.
        let status = sqlx::query_scalar::<_, String>(
            r#"
            INSERT INTO booking_candidates (
                workspace_id, target_kind, display_name, city_slug,
                route_kind, route_value, source, source_reference,
                evidence, fit_basis_points, status
            ) VALUES ($1,$2,$3,$4,'email',$5,'contact_scan',$6,$7,6000,'admitted')
            ON CONFLICT (workspace_id, route_kind, lower(btrim(route_value))) DO UPDATE SET
                city_slug = COALESCE(booking_candidates.city_slug, EXCLUDED.city_slug)
            RETURNING status
            "#,
        )
        .bind(workspace_id)
        .bind(target_kind)
        .bind(&display_name)
        .bind(&city_slug)
        .bind(&contact.normalized_email)
        // source_reference names the thread/file the address came from — the
        // reviewer must be able to place who this is, not trust the kind.
        .bind(
            contact
                .source_file_name
                .chars()
                .take(500)
                .collect::<String>(),
        )
        .bind(&evidence)
        .fetch_one(&mut *tx)
        .await?;
        if status == "refused" {
            // The route was durably refused before — a promote must not
            // quietly overturn that, and the contact must not read as
            // resolved when nothing actionable exists.
            tx.rollback().await?;
            return Ok(BookingPromoteOutcome::RouteRefused);
        }
        sqlx::query(
            "UPDATE drive_contacts SET beacon_outcome = 'promoted' WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(contact.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(BookingPromoteOutcome::Done)
    }

    /// Marks a contact's fan outcome promoted. The fan write itself goes
    /// through `fan_import::import_batch` in the API layer — this only
    /// records the decision so a re-scan does not re-suggest.
    pub async fn mark_fan_promoted(
        &self,
        workspace_id: Uuid,
        contact_id: Uuid,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            "UPDATE drive_contacts SET fan_outcome = 'promoted' WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(contact_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Connections due for a sync: status connected on the given intake
    /// platform ('gdrive' | 'gmail'). Returns (id, external_account_ref) —
    /// tokens are decrypted in the worker at point of use.
    /// `(id, account_ref, scan_scope)` — the scope is the tenant's chosen
    /// read boundary; NULL means the question was never answered and the
    /// worker must scan nothing rather than default to everything.
    pub async fn due_connections(
        &self,
        workspace_id: Uuid,
        platform: &str,
    ) -> Result<Vec<(Uuid, String, Option<serde_json::Value>)>, GDriveError> {
        sqlx::query_as::<_, (Uuid, String, Option<serde_json::Value>)>(
            r#"
            SELECT id, external_account_ref, scan_scope
            FROM fanbase_connections
            WHERE workspace_id = $1 AND platform = $2 AND status = 'connected'
            "#,
        )
        .bind(workspace_id)
        .bind(platform)
        .fetch_all(&self.pool)
        .await
        .map_err(GDriveError::Database)
    }

    /// Encrypted token triple for the worker's point-of-use decrypt.
    pub async fn connection_tokens(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
    ) -> Result<(Option<String>, Option<String>, Option<time::OffsetDateTime>), GDriveError> {
        sqlx::query_as::<_, (Option<String>, Option<String>, Option<time::OffsetDateTime>)>(
            r#"SELECT encrypted_access_token, encrypted_refresh_token, token_expires_at
               FROM fanbase_connections WHERE workspace_id = $1 AND id = $2"#,
        )
        .bind(workspace_id)
        .bind(connection_id)
        .fetch_one(&self.pool)
        .await
        .map_err(GDriveError::Database)
    }

    /// Rotated tokens after a refresh — same encrypted columns.
    pub async fn store_refreshed_access_token(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        encrypted_access_token: &str,
        expires_at: time::OffsetDateTime,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            "UPDATE fanbase_connections SET encrypted_access_token = $3, token_expires_at = $4, updated_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(connection_id)
        .bind(encrypted_access_token)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_sync_ok(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            "UPDATE fanbase_connections SET last_sync_at = now(), last_sync_error = NULL, updated_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(connection_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_sync_error(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        error: &str,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            "UPDATE fanbase_connections SET last_sync_failed_at = now(), last_sync_error = $3, updated_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(connection_id)
        .bind(error.chars().take(500).collect::<String>())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The connection's incremental cursor — Gmail stores its last
    /// historyId here; Drive leaves it NULL (per-file mtimes cover it).
    pub async fn sync_cursor(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
    ) -> Result<Option<String>, GDriveError> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT sync_cursor FROM fanbase_connections WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(connection_id)
        .fetch_one(&self.pool)
        .await
        .map_err(GDriveError::Database)
    }

    /// Advances the cursor only after a cycle's upserts committed — a crash
    /// before this write re-reads the same history page next cycle, and the
    /// email dedup makes the re-read a no-op.
    ///
    /// The write is compare-and-swap on the cursor the cycle started from:
    /// a scope change clears `sync_cursor`, and an in-flight cycle that read
    /// the old cursor must not resurrect it — that would cancel the new
    /// boundary's fresh sweep. A CAS miss is silent on purpose: the scope
    /// change wins and the next cycle re-sweeps under it.
    pub async fn set_sync_cursor(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        cursor: &str,
        expected: Option<&str>,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            r#"
            UPDATE fanbase_connections
            SET sync_cursor = $3, updated_at = now()
            WHERE workspace_id = $1 AND id = $2
              AND sync_cursor IS NOT DISTINCT FROM $4
            "#,
        )
        .bind(workspace_id)
        .bind(connection_id)
        .bind(cursor)
        .bind(expected)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A revoked grant is not transient: flip the connection to `expired`
    /// so the tile asks for a reconnect instead of failing every hour.
    pub async fn mark_expired(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        error: &str,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            "UPDATE fanbase_connections SET status = 'expired', last_sync_failed_at = now(), last_sync_error = $3, updated_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(connection_id)
        .bind(error.chars().take(500).collect::<String>())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Files a contact on the representation list — the `outreach_targets`
/// row an approach can be requested against. Shared by the two beacon promote
/// paths: an `agent`/`label` sheet kind files it directly, and a
/// `booking_agent`/`talent_buyer` files it beside the registry row so the
/// agent is approachable rather than merely recorded.
///
/// Verified because the band's own records are the route's provenance;
/// `accepts_outreach` stays false — the consent basis is the operator's to
/// state before an approach may send. A re-import refreshes the name and
/// retires a stale representation kind, but never rewrites a non-
/// representation kind at the same address — a contact filed as press stays
/// press.
async fn upsert_representation_target(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    target_kind: &str,
    display_name: &str,
    contact_email: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO outreach_targets
            (workspace_id, target_kind, display_name, contact_email,
             active, verified, accepts_outreach, do_not_contact)
        VALUES ($1, $2, $3, $4, true, true, false, false)
        ON CONFLICT (workspace_id, contact_email) DO UPDATE SET
            target_kind = CASE
                WHEN outreach_targets.target_kind IN ('agent','label')
                    THEN EXCLUDED.target_kind
                ELSE outreach_targets.target_kind
            END,
            display_name = EXCLUDED.display_name,
            updated_at = now()
        "#,
    )
    .bind(workspace_id)
    .bind(target_kind)
    .bind(display_name)
    .bind(contact_email)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
