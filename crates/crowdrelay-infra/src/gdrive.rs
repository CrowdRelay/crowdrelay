//! Google Drive contacts staging — `viryaos_drive_contacts`.
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
    pub notes: Option<String>,
    pub source_file_id: String,
    pub source_file_name: String,
    pub sources: Vec<String>,
    pub last_seen_at: time::OffsetDateTime,
    pub disappeared_at: Option<time::OffsetDateTime>,
    pub fan_outcome: String,
    pub beacon_outcome: String,
}

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
            "SELECT last_mtime FROM viryaos_drive_files WHERE workspace_id = $1 AND file_id = $2",
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
            INSERT INTO viryaos_drive_files
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

        // source_file_name is capped at 500 by CHECK.
        let ref_name: String = ref_name.chars().take(500).collect();

        let mut upserted = 0u64;
        for contact in ordered {
            sqlx::query(
                r#"
                INSERT INTO viryaos_drive_contacts (
                    workspace_id, normalized_email, display_name, organization,
                    phone, suggested_kind, notes, source_file_id, source_file_name,
                    sources, last_seen_at, disappeared_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, ARRAY[$10]::text[], now(), NULL)
                ON CONFLICT (workspace_id, normalized_email) DO UPDATE SET
                    display_name = COALESCE(EXCLUDED.display_name, viryaos_drive_contacts.display_name),
                    organization = COALESCE(EXCLUDED.organization, viryaos_drive_contacts.organization),
                    phone = COALESCE(EXCLUDED.phone, viryaos_drive_contacts.phone),
                    suggested_kind = COALESCE(EXCLUDED.suggested_kind, viryaos_drive_contacts.suggested_kind),
                    notes = COALESCE(EXCLUDED.notes, viryaos_drive_contacts.notes),
                    source_file_id = EXCLUDED.source_file_id,
                    source_file_name = EXCLUDED.source_file_name,
                    sources = (SELECT array_agg(DISTINCT s) FROM unnest(
                        viryaos_drive_contacts.sources || EXCLUDED.sources) AS s),
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
            .bind(&contact.notes)
            .bind(ref_id)
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
            UPDATE viryaos_drive_contacts
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
            .bind(ref_id)
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

    /// The review queue: staged rows first, newest seen first. `outcome`
    /// filters on either flag — `staged` means "still undecided somewhere".
    pub async fn list_contacts(
        &self,
        workspace_id: Uuid,
        limit: i64,
    ) -> Result<Vec<DriveContactRow>, GDriveError> {
        sqlx::query_as::<_, DriveContactRow>(
            r#"
            SELECT id, normalized_email, display_name, organization, phone,
                   suggested_kind, notes, source_file_id, source_file_name, sources,
                   last_seen_at, disappeared_at, fan_outcome, beacon_outcome
            FROM viryaos_drive_contacts
            WHERE workspace_id = $1
            ORDER BY (fan_outcome = 'staged' OR beacon_outcome = 'staged') DESC,
                     last_seen_at DESC
            LIMIT $2
            "#,
        )
        .bind(workspace_id)
        .bind(limit)
        .fetch_all(&self.pool)
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
        let sql = format!(
            r#"
            UPDATE viryaos_drive_contacts SET {column} = $3
            WHERE workspace_id = $1 AND id = $2
            RETURNING id, normalized_email, display_name, organization, phone,
                      suggested_kind, notes, source_file_id, source_file_name, sources,
                      last_seen_at, disappeared_at, fan_outcome, beacon_outcome
            "#
        );
        sqlx::query_as::<_, DriveContactRow>(&sql)
            .bind(workspace_id)
            .bind(contact_id)
            .bind(outcome)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(GDriveError::NotFound)
    }

    /// One staged contact by id, for the promote handlers.
    pub async fn get_contact(
        &self,
        workspace_id: Uuid,
        contact_id: Uuid,
    ) -> Result<DriveContactRow, GDriveError> {
        sqlx::query_as::<_, DriveContactRow>(
            r#"
            SELECT id, normalized_email, display_name, organization, phone,
                   suggested_kind, notes, source_file_id, source_file_name, sources,
                   last_seen_at, disappeared_at, fan_outcome, beacon_outcome
            FROM viryaos_drive_contacts
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(contact_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(GDriveError::NotFound)
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
                 contact_domain, why_fit, evidence, status)
            VALUES ($1,$2,$3,$4,$5,$6,$7,'proposed')
            ON CONFLICT (workspace_id, display_name, target_kind) DO UPDATE SET
                contact_email = COALESCE(EXCLUDED.contact_email, agent_outreach_targets.contact_email),
                contact_domain = COALESCE(EXCLUDED.contact_domain, agent_outreach_targets.contact_domain),
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
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE viryaos_drive_contacts SET beacon_outcome = 'promoted' WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(contact.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Promotes a contact into booking supply: a `viryaos_booking_candidates`
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
                // Only a single-city match resolves: two rooms sharing the
                // name pick nothing — guessing a city files the candidate
                // against the wrong place.
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
                        sqlx::query_scalar::<_, String>("SELECT slug FROM cities WHERE id = $1")
                            .bind(*only)
                            .fetch_optional(&self.pool)
                            .await?
                    }
                    _ => None,
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
            INSERT INTO viryaos_booking_candidates (
                workspace_id, target_kind, display_name, city_slug,
                route_kind, route_value, source, source_reference,
                evidence, fit_basis_points, status
            ) VALUES ($1,$2,$3,$4,'email',$5,'contact_scan',$6,$7,6000,'admitted')
            ON CONFLICT (workspace_id, route_kind, lower(btrim(route_value))) DO UPDATE SET
                city_slug = COALESCE(viryaos_booking_candidates.city_slug, EXCLUDED.city_slug)
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
            "UPDATE viryaos_drive_contacts SET beacon_outcome = 'promoted' WHERE workspace_id = $1 AND id = $2",
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
            "UPDATE viryaos_drive_contacts SET fan_outcome = 'promoted' WHERE workspace_id = $1 AND id = $2",
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
    pub async fn due_connections(
        &self,
        workspace_id: Uuid,
        platform: &str,
    ) -> Result<Vec<(Uuid, String)>, GDriveError> {
        sqlx::query_as::<_, (Uuid, String)>(
            r#"
            SELECT id, external_account_ref
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
    pub async fn set_sync_cursor(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        cursor: &str,
    ) -> Result<(), GDriveError> {
        sqlx::query(
            "UPDATE fanbase_connections SET sync_cursor = $3, updated_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(connection_id)
        .bind(cursor)
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
