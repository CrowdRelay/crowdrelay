async fn persist_success(
    pool: &PgPool,
    config: &EventSyncWorkerConfig,
    source: &EventSourceRow,
    sync_started_at: OffsetDateTime,
    events: &[NormalizedExternalEvent],
) -> Result<(), EventSyncError> {
    let mut transaction = pool.begin().await.map_err(EventSyncError::sqlx)?;
    configure_transaction(&mut transaction, config).await?;

    sqlx::query(
        r#"
        SELECT id
        FROM event_sources
        WHERE workspace_id = $1
          AND id = $2
          AND sync_lease_owner = $3
          AND sync_lease_until > now()
        FOR UPDATE
        "#,
    )
    .bind(source.workspace_id)
    .bind(source.id)
    .bind(
        source
            .sync_lease_owner
            .ok_or(EventSyncError::InvalidSource)?,
    )
    .fetch_optional(&mut *transaction)
    .await
    .map_err(EventSyncError::sqlx)?
    .ok_or(EventSyncError::InvalidSource)?;

    let mut seen = HashSet::with_capacity(events.len());
    for event in events {
        if !seen.insert(event.source_event_id.as_str()) {
            continue;
        }
        let city_id = upsert_city(&mut transaction, event).await?;
        let copy_source_hash = event_copy_source_hash(event);
        let upserted = upsert_event(
            &mut transaction,
            source,
            event,
            city_id,
            sync_started_at,
            &copy_source_hash,
        )
        .await?;
        enqueue_copy_enrichment(
            &mut transaction,
            source,
            event,
            upserted.current.id,
            &copy_source_hash,
        )
        .await?;
        if should_announce_new_event(
            source.last_success_at.is_some(),
            upserted.inserted,
            upserted.current.starts_at,
            sync_started_at,
        ) {
            announce_new_event(
                &mut transaction,
                source,
                upserted.current.id,
                event,
                city_id,
            )
            .await?;
        } else if source.last_success_at.is_some()
            && upserted.current.starts_at > sync_started_at
            && let Some(previous) = upserted.previous.as_ref()
        {
            announce_event_change(
                &mut transaction,
                source,
                "updated",
                &upserted.current,
                Some(previous),
            )
            .await?;
        }
    }

    // One successful empty response is treated as transient. A second empty
    // response in a row is authoritative, so a genuinely empty calendar does
    // not preserve cancelled concerts forever.
    let empty_response = events.is_empty();
    let authoritative = !empty_response || source.consecutive_empty_syncs >= 1;
    let cancelled_events = if authoritative {
        let cancelled_ids = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE events
            SET status = 'cancelled'
            WHERE workspace_id = $1
              AND source_id = $2
              AND source_last_seen_at < $3
              AND starts_at >= now()
              AND status = 'published'
            RETURNING id
            "#,
        )
        .bind(source.workspace_id)
        .bind(source.id)
        .bind(sync_started_at)
        .fetch_all(&mut *transaction)
        .await
        .map_err(EventSyncError::sqlx)?;
        load_event_snapshots(&mut transaction, source.workspace_id, &cancelled_ids).await?
    } else {
        Vec::new()
    };
    if source.last_success_at.is_some() {
        for event in &cancelled_events {
            announce_event_change(&mut transaction, source, "cancelled", event, None).await?;
        }
    }
    let cancelled = cancelled_events.len();

    sqlx::query(
        r#"
        UPDATE event_sources
        SET last_synced_at = now(),
            last_success_at = now(),
            sync_lease_until = NULL,
            sync_lease_owner = NULL,
            consecutive_failures = 0,
            consecutive_empty_syncs = CASE
                WHEN $3 THEN consecutive_empty_syncs + 1
                ELSE 0
            END,
            last_error = NULL,
            next_sync_at = now() + (sync_interval_seconds::bigint * interval '1 second')
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(source.workspace_id)
    .bind(source.id)
    .bind(empty_response)
    .execute(&mut *transaction)
    .await
    .map_err(EventSyncError::sqlx)?;

    sqlx::query(
        "INSERT INTO audit_events (workspace_id, actor_kind, action, target_type, target_id, metadata) VALUES ($1, 'system', 'event_source.synced', 'event_source', $2, $3)",
    )
    .bind(source.workspace_id)
    .bind(source.id.to_string())
    .bind(json!({
        "provider": &source.provider,
        "artist_name": &source.artist_name,
        "received_events": events.len(),
        "cancelled_missing_events": cancelled,
        "empty_response": empty_response,
        "authoritative_response": authoritative,
        "consecutive_empty_syncs": if empty_response { source.consecutive_empty_syncs.saturating_add(1) } else { 0 },
        "sync_started_at": sync_started_at,
    }))
    .execute(&mut *transaction)
    .await
    .map_err(EventSyncError::sqlx)?;

    transaction.commit().await.map_err(EventSyncError::sqlx)?;
    Ok(())
}
async fn upsert_city(
    transaction: &mut Transaction<'_, Postgres>,
    event: &NormalizedExternalEvent,
) -> Result<Option<Uuid>, EventSyncError> {
    let (Some(name), Some(slug)) = (&event.city_name, &event.city_slug) else {
        return Ok(None);
    };
    let city_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO cities (slug, name, country_code, region, latitude, longitude)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (country_code, slug) DO UPDATE
        SET name = EXCLUDED.name,
            region = COALESCE(EXCLUDED.region, cities.region),
            latitude = COALESCE(EXCLUDED.latitude, cities.latitude),
            longitude = COALESCE(EXCLUDED.longitude, cities.longitude)
        RETURNING id
        "#,
    )
    .bind(slug)
    .bind(name)
    .bind(&event.country_code)
    .bind(&event.region)
    .bind(event.latitude)
    .bind(event.longitude)
    .fetch_one(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    Ok(Some(city_id))
}

async fn upsert_event(
    transaction: &mut Transaction<'_, Postgres>,
    source: &EventSourceRow,
    event: &NormalizedExternalEvent,
    city_id: Option<Uuid>,
    sync_started_at: OffsetDateTime,
    copy_source_hash: &[u8; 32],
) -> Result<EventUpsertResult, EventSyncError> {
    let previous = sqlx::query_as::<_, PersistedEventSnapshot>(
        r#"
        SELECT
            event.id, event.city_id, city.name AS city_name,
            city.country_code, event.slug, event.title, event.description,
            event.venue, event.venue_address, event.timezone, event.starts_at,
            event.ticket_url, event.external_event_url, event.status
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1
          AND (
              (event.source_id = $2 AND event.source_event_id = $3)
              OR (
                  $4::text IS NOT NULL
                  AND event.external_event_url = $4
                  AND (event.source_id IS NULL OR event.source_id = $2)
              )
              OR (
                  event.source_id IS NULL
                  AND abs(extract(epoch FROM (event.starts_at - $5))) <= 10800
                  AND (
                      ($6::text IS NOT NULL AND lower(btrim(event.venue)) = lower(btrim($6)))
                      OR ($7::uuid IS NOT NULL AND event.city_id = $7)
                  )
              )
          )
        ORDER BY
            (event.source_id = $2 AND event.source_event_id = $3) DESC,
            ($4::text IS NOT NULL AND event.external_event_url = $4) DESC,
            event.id
        LIMIT 1
        FOR UPDATE OF event
        "#,
    )
    .bind(source.workspace_id)
    .bind(source.id)
    .bind(&event.source_event_id)
    .bind(&event.external_event_url)
    .bind(event.starts_at)
    .bind(&event.venue)
    .bind(city_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;

    if let Some(previous) = previous {
        sqlx::query(
            r#"
            UPDATE events
            SET city_id = CASE
                    WHEN source_id IS NULL AND city_id IS NOT NULL THEN city_id
                    ELSE COALESCE($3, city_id)
                END,
                title = CASE
                    WHEN source_id IS NULL AND btrim(title) <> '' THEN title
                    ELSE $4
                END,
                source_description = $5,
                description = CASE
                    WHEN description_origin = 'manual' THEN description
                    WHEN description_origin = 'ai' AND description_source_hash = $16 THEN description
                    ELSE COALESCE($5, description)
                END,
                description_origin = CASE
                    WHEN description_origin = 'manual' THEN 'manual'
                    WHEN description_origin = 'ai' AND description_source_hash = $16 THEN 'ai'
                    ELSE 'provider'
                END,
                description_source_hash = CASE
                    WHEN description_origin = 'manual' THEN description_source_hash
                    ELSE $16
                END,
                description_language = CASE
                    WHEN description_origin = 'manual' THEN description_language
                    ELSE 'pl'
                END,
                venue = CASE
                    WHEN source_id IS NULL AND venue IS NOT NULL THEN venue
                    ELSE COALESCE($6, venue)
                END,
                venue_address = CASE
                    WHEN source_id IS NULL AND venue_address IS NOT NULL THEN venue_address
                    ELSE COALESCE($7, venue_address)
                END,
                timezone = CASE WHEN source_id IS NULL THEN timezone ELSE $8 END,
                starts_at = $9,
                ticket_url = COALESCE($10, ticket_url),
                external_event_url = CASE
                    WHEN source_id IS NULL AND external_event_url IS NOT NULL THEN external_event_url
                    ELSE COALESCE($11, external_event_url)
                END,
                status = 'published',
                published_at = COALESCE(published_at, now()),
                source_id = $12,
                source_provider = $13,
                source_event_id = $14,
                source_last_seen_at = $15
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(source.workspace_id)
        .bind(previous.id)
        .bind(city_id)
        .bind(&event.title)
        .bind(&event.description)
        .bind(&event.venue)
        .bind(&event.venue_address)
        .bind(&event.timezone)
        .bind(event.starts_at)
        .bind(&event.ticket_url)
        .bind(&event.external_event_url)
        .bind(source.id)
        .bind(&source.provider)
        .bind(&event.source_event_id)
        .bind(sync_started_at)
        .bind(copy_source_hash.as_slice())
        .execute(&mut **transaction)
        .await
        .map_err(EventSyncError::sqlx)?;
        let current = load_event_snapshot(transaction, source.workspace_id, previous.id).await?;
        let previous = meaningful_event_change(&previous, &current).then_some(previous);
        return Ok(EventUpsertResult {
            current,
            previous,
            inserted: false,
        });
    }

    let inserted = sqlx::query_as::<_, InsertedEventRow>(
        r#"
        INSERT INTO events (
            workspace_id, city_id, slug, title, description, source_description,
            description_origin, description_source_hash, description_language,
            venue, venue_address, timezone, starts_at, ticket_url,
            external_event_url, status, published_at,
            source_id, source_provider, source_event_id, source_last_seen_at
        )
        VALUES (
            $1, $2, $3, $4, $5, $5,
            'provider', $16, 'pl',
            $6, $7, $8, $9, $10,
            $11, 'published', now(), $12, $13, $14, $15
        )
        ON CONFLICT (workspace_id, source_id, source_event_id)
            WHERE source_id IS NOT NULL
        DO UPDATE SET
            city_id = EXCLUDED.city_id,
            title = EXCLUDED.title,
            source_description = EXCLUDED.source_description,
            description = CASE
                WHEN events.description_origin = 'manual' THEN events.description
                WHEN events.description_origin = 'ai'
                     AND events.description_source_hash = EXCLUDED.description_source_hash
                    THEN events.description
                ELSE COALESCE(EXCLUDED.source_description, events.description)
            END,
            description_origin = CASE
                WHEN events.description_origin = 'manual' THEN 'manual'
                WHEN events.description_origin = 'ai'
                     AND events.description_source_hash = EXCLUDED.description_source_hash
                    THEN 'ai'
                ELSE 'provider'
            END,
            description_source_hash = CASE
                WHEN events.description_origin = 'manual' THEN events.description_source_hash
                ELSE EXCLUDED.description_source_hash
            END,
            venue = EXCLUDED.venue,
            venue_address = COALESCE(EXCLUDED.venue_address, events.venue_address),
            timezone = EXCLUDED.timezone,
            starts_at = EXCLUDED.starts_at,
            ticket_url = COALESCE(EXCLUDED.ticket_url, events.ticket_url),
            external_event_url = COALESCE(EXCLUDED.external_event_url, events.external_event_url),
            status = 'published',
            published_at = COALESCE(events.published_at, now()),
            source_last_seen_at = EXCLUDED.source_last_seen_at
        RETURNING id, (xmax = 0) AS inserted
        "#,
    )
    .bind(source.workspace_id)
    .bind(city_id)
    .bind(&event.slug)
    .bind(&event.title)
    .bind(&event.description)
    .bind(&event.venue)
    .bind(&event.venue_address)
    .bind(&event.timezone)
    .bind(event.starts_at)
    .bind(&event.ticket_url)
    .bind(&event.external_event_url)
    .bind(source.id)
    .bind(&source.provider)
    .bind(&event.source_event_id)
    .bind(sync_started_at)
    .bind(copy_source_hash.as_slice())
    .fetch_one(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    let current = load_event_snapshot(transaction, source.workspace_id, inserted.id).await?;
    Ok(EventUpsertResult {
        current,
        previous: None,
        inserted: inserted.inserted,
    })
}
