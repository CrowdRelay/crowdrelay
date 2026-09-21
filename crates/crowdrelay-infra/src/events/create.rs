// The manual show entry — split out of `events.rs` under the same modularity
// contract as `festival.rs`. Included, not a module: it is one method on
// `PostgresEventRepository` and needs the struct's private pool.
//
// Until this existed the `events` table only grew from sync sources —
// Bandsintown, Ticketmaster, an OSM sweep. A tenant whose label runs its own
// calendar had no way to put a night in the system, and every show surface
// (checklists, the T+7 report, gig planning's `has_upcoming_show`) reads this
// table — so the missing row hid the whole capability, not just the list.

/// Every field the write is decided by — a replay with the same key but a
/// different night (or a different publish answer) is a different request,
/// not a retry, and refuses rather than silently answering the first call.
#[derive(Serialize)]
struct CreateFingerprint<'a> {
    workspace_id: WorkspaceId,
    title: &'a str,
    timezone: Option<&'a str>,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
    venue: Option<&'a str>,
    venue_address: Option<&'a str>,
    city_name: Option<&'a str>,
    city_country_code: Option<&'a str>,
    city_region: Option<&'a str>,
    ticket_url: Option<&'a str>,
    publish: bool,
}

fn create_request_hash(command: &CreateEventCommand) -> Result<Vec<u8>, EventStoreError> {
    let fingerprint = CreateFingerprint {
        workspace_id: command.workspace_id,
        title: &command.title,
        timezone: command.timezone.as_deref(),
        starts_at: command.starts_at,
        doors_at: command.doors_at,
        ends_at: command.ends_at,
        venue: command.venue.as_deref(),
        venue_address: command.venue_address.as_deref(),
        city_name: command.city_name.as_deref(),
        city_country_code: command.city_country_code.as_deref(),
        city_region: command.city_region.as_deref(),
        ticket_url: command.ticket_url.as_deref(),
        publish: command.publish,
    };
    let encoded = serde_json::to_vec(&fingerprint).map_err(|_| EventStoreError::Unexpected)?;
    Ok(Sha256::digest(encoded).to_vec())
}

impl PostgresEventRepository {
    async fn create_event_inner(
        &self,
        command: &CreateEventCommand,
    ) -> Result<CreatedEvent, EventStoreError> {
        let request_hash = create_request_hash(command)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(EventStoreError::from_sqlx)?;
        let workspace_id =
            trusted_workspace_id_in_transaction(&mut transaction, &self.workspace_slug).await?;
        if workspace_id != command.workspace_id {
            return Err(EventStoreError::NotFound);
        }

        // Same replay protocol as `register_interest`: the control plane
        // forwards the operator's key, and a retried submit must answer the
        // show it already booked — not a second one.
        let inserted = start_idempotency(
            &mut transaction,
            workspace_id,
            CREATE_IDEMPOTENCY_SCOPE,
            command.idempotency_key.as_str(),
            command.request_id.as_str(),
            &request_hash,
            self.operation_timeout,
        )
        .await?;
        if !inserted {
            let row = lock_idempotency(
                &mut transaction,
                workspace_id,
                CREATE_IDEMPOTENCY_SCOPE,
                command.idempotency_key.as_str(),
            )
            .await?;
            if row.request_hash != request_hash {
                return Err(EventStoreError::Conflict);
            }
            if row.state == "completed" {
                let response = row.response_body.ok_or(EventStoreError::Unexpected)?;
                let result =
                    serde_json::from_str(&response).map_err(|_| EventStoreError::Unexpected)?;
                transaction
                    .commit()
                    .await
                    .map_err(EventStoreError::from_sqlx)?;
                return Ok(result);
            }
            if !row.lease_expired {
                return Err(EventStoreError::Conflict);
            }
            reclaim_idempotency(
                &mut transaction,
                workspace_id,
                CREATE_IDEMPOTENCY_SCOPE,
                command.idempotency_key.as_str(),
                command.request_id.as_str(),
                self.operation_timeout,
            )
            .await?;
        }

        // The city registry is deliberately workspace-less — cities are shared
        // geography, not tenant data. A name the sync never saw becomes a row
        // here so the show joins the same city the gig planner ranks.
        let city_id = match (&command.city_name, &command.city_country_code) {
            (Some(name), Some(country)) => {
                let slug = slugify(name).ok_or(EventStoreError::Conflict)?;
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    INSERT INTO cities (slug, name, country_code, region)
                    VALUES ($1, $2, $3, $4)
                    ON CONFLICT (country_code, slug) DO UPDATE
                    SET name = EXCLUDED.name,
                        region = COALESCE(EXCLUDED.region, cities.region)
                    RETURNING id
                    "#,
                )
                .bind(&slug)
                .bind(name)
                .bind(country)
                .bind(command.city_region.as_deref())
                .fetch_one(&mut *transaction)
                .await
                .map_err(EventStoreError::from_sqlx)
                .map(Some)?
            }
            _ => None,
        };

        // The venue's own clock: the operator's answer beats the workspace
        // setting beats the column default ('Europe/Warsaw' — correct for the
        // first tenant, wrong to inherit silently for the next).
        let timezone = match command.timezone.as_deref() {
            Some(zone) => zone.to_owned(),
            None => sqlx::query_scalar::<_, String>(
                "SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'crew_timezone'",
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(EventStoreError::from_sqlx)?
            .unwrap_or_else(|| "Europe/Warsaw".to_owned()),
        };

        let stem = slugify(&command.slug_base).unwrap_or_else(|| "show".to_owned());
        let mut created: Option<(Uuid, String)> = None;
        for attempt in 0..MAX_EVENT_SLUG_ATTEMPTS {
            let slug = if attempt == 0 {
                stem.clone()
            } else {
                format!("{stem}-{}", attempt + 1)
            };
            let row = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO events (
                    workspace_id, city_id, slug, title, venue, venue_address,
                    timezone, starts_at, doors_at, ends_at, ticket_url,
                    status, published_at
                ) VALUES (
                    $1, $2, $3, $4, $5, $6,
                    $7, $8, $9, $10, $11,
                    $12, CASE WHEN $12 = 'published' THEN now() END
                )
                ON CONFLICT (workspace_id, slug) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(city_id)
            .bind(&slug)
            .bind(&command.title)
            .bind(command.venue.as_deref())
            .bind(command.venue_address.as_deref())
            .bind(&timezone)
            .bind(command.starts_at)
            .bind(command.doors_at)
            .bind(command.ends_at)
            .bind(command.ticket_url.as_deref())
            .bind(if command.publish { "published" } else { "draft" })
            .fetch_optional(&mut *transaction)
            .await
            .map_err(EventStoreError::from_sqlx)?;
            if let Some(id) = row {
                created = Some((id, slug));
                break;
            }
        }
        let (event_id, slug) = created.ok_or(EventStoreError::Conflict)?;

        let result = CreatedEvent {
            event_id: EventId::from_uuid(event_id),
            slug,
            status: if command.publish {
                "published".to_owned()
            } else {
                "draft".to_owned()
            },
        };
        complete_idempotency(
            &mut transaction,
            workspace_id,
            CREATE_IDEMPOTENCY_SCOPE,
            command.idempotency_key.as_str(),
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(EventStoreError::from_sqlx)?;
        Ok(result)
    }
}
