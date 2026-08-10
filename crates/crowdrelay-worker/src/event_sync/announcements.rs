fn event_copy_source_hash(event: &NormalizedExternalEvent) -> [u8; 32] {
    let canonical = serde_json::to_vec(&json!({
        "title": event.title,
        "description": event.description,
        "city": event.city_name,
        "country_code": event.country_code,
        "region": event.region,
        "venue": event.venue,
        "venue_address": event.venue_address,
        "timezone": event.timezone,
        "starts_at": event.starts_at,
        "ticket_url": event.ticket_url,
        "external_event_url": event.external_event_url,
    }))
    .unwrap_or_default();
    Sha256::digest(canonical).into()
}
async fn enqueue_copy_enrichment(
    transaction: &mut Transaction<'_, Postgres>,
    source: &EventSourceRow,
    event: &NormalizedExternalEvent,
    event_id: Uuid,
    source_hash: &[u8; 32],
) -> Result<(), EventSyncError> {
    sqlx::query(
        r#"
        UPDATE event_copy_enrichments
        SET status = 'stale',
            rejection_reason = 'Bandsintown source facts changed',
            completed_at = now()
        WHERE workspace_id = $1
          AND event_id = $2
          AND status = 'pending'
          AND source_hash <> $3
        "#,
    )
    .bind(source.workspace_id)
    .bind(event_id)
    .bind(source_hash.as_slice())
    .execute(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;

    let enrichment_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO event_copy_enrichments (
            workspace_id, event_id, source_hash, language
        )
        SELECT $1, $2, $3, 'pl'
        FROM events
        WHERE workspace_id = $1
          AND id = $2
          AND description_origin <> 'manual'
        ON CONFLICT (workspace_id, event_id, source_hash, language) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(source.workspace_id)
    .bind(event_id)
    .bind(source_hash.as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    let Some(enrichment_id) = enrichment_id else {
        return Ok(());
    };

    append_event_outbox(
        transaction,
        source.workspace_id,
        "event.copy.enrichment_requested",
        &format!("event-copy:{enrichment_id}"),
        json!({
            "enrichment_id": enrichment_id,
            "source_hash": hex::encode(source_hash),
            "language": "pl",
            "event": {
                "id": event_id,
                "slug": event.slug,
                "title": event.title,
                "source_description": event.description,
                "city": event.city_name,
                "country_code": event.country_code,
                "region": event.region,
                "venue": event.venue,
                "venue_address": event.venue_address,
                "timezone": event.timezone,
                "starts_at": event.starts_at,
                "ticket_url": event.ticket_url,
                "bandsintown_event_url": event.external_event_url,
            },
            "source": {
                "provider": source.provider,
                "artist_name": source.artist_name,
            },
        }),
    )
    .await
}

async fn load_event_snapshots(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event_ids: &[Uuid],
) -> Result<Vec<PersistedEventSnapshot>, EventSyncError> {
    if event_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, PersistedEventSnapshot>(
        r#"
        SELECT
            event.id, event.city_id, city.name AS city_name,
            city.country_code, event.slug, event.title, event.description,
            event.venue, event.venue_address, event.timezone, event.starts_at,
            event.ticket_url, event.external_event_url, event.status
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1
          AND event.id = ANY($2)
        ORDER BY event.starts_at, event.id
        "#,
    )
    .bind(workspace_id)
    .bind(event_ids)
    .fetch_all(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)
}

async fn load_event_snapshot(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event_id: Uuid,
) -> Result<PersistedEventSnapshot, EventSyncError> {
    sqlx::query_as::<_, PersistedEventSnapshot>(
        r#"
        SELECT
            event.id, event.city_id, city.name AS city_name,
            city.country_code, event.slug, event.title, event.description,
            event.venue, event.venue_address, event.timezone, event.starts_at,
            event.ticket_url, event.external_event_url, event.status
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1 AND event.id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)
}

fn meaningful_event_change(
    previous: &PersistedEventSnapshot,
    current: &PersistedEventSnapshot,
) -> bool {
    previous.city_id != current.city_id
        || previous.starts_at != current.starts_at
        || previous.venue != current.venue
        || previous.venue_address != current.venue_address
        || previous.status != current.status
}

fn should_announce_new_event(
    has_prior_success: bool,
    inserted: bool,
    starts_at: OffsetDateTime,
    sync_started_at: OffsetDateTime,
) -> bool {
    has_prior_success && inserted && starts_at > sync_started_at
}

async fn announce_new_event(
    transaction: &mut Transaction<'_, Postgres>,
    source: &EventSourceRow,
    event_id: Uuid,
    event: &NormalizedExternalEvent,
    city_id: Option<Uuid>,
) -> Result<(), EventSyncError> {
    let announcement_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO event_announcements (workspace_id, event_id, kind, fingerprint)
        VALUES ($1, $2, 'published', 'initial')
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(source.workspace_id)
    .bind(event_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    let Some(announcement_id) = announcement_id else {
        return Ok(());
    };

    append_delayed_event_outbox(
        transaction,
        source.workspace_id,
        "event.published",
        &format!("event:{event_id}:published"),
        json!({
            "announcement_id": announcement_id,
            "event": {
                "id": event_id,
                "slug": event.slug,
                "title": event.title,
                "description": event.description,
                "city": event.city_name,
                "country_code": event.country_code,
                "venue": event.venue,
                "venue_address": event.venue_address,
                "timezone": event.timezone,
                "starts_at": event.starts_at,
                "ticket_url": event.ticket_url,
                "bandsintown_event_url": event.external_event_url,
            },
            "source": {
                "provider": source.provider,
                "artist_name": source.artist_name,
            },
        }),
    )
    .await?;
    append_delayed_event_outbox(
        transaction,
        source.workspace_id,
        "event.discord_report_due",
        &format!("event:{event_id}:discord"),
        json!({
            "announcement_id": announcement_id,
            "event": {
                "id": event_id,
                "slug": event.slug,
                "title": event.title,
                "description": event.description,
                "city": event.city_name,
                "country_code": event.country_code,
                "venue": event.venue,
                "venue_address": event.venue_address,
                "timezone": event.timezone,
                "starts_at": event.starts_at,
                "ticket_url": event.ticket_url,
                "bandsintown_event_url": event.external_event_url,
            },
            "source": {
                "provider": source.provider,
                "artist_name": source.artist_name,
            },
        }),
    )
    .await?;

    let recipient_count = enqueue_regional_announcement_outbox(
        transaction,
        source.workspace_id,
        announcement_id,
        city_id,
        event.latitude,
        event.longitude,
        json!({
            "id": event_id,
            "slug": event.slug,
            "title": event.title,
            "description": event.description,
            "city": event.city_name,
            "country_code": event.country_code,
            "venue": event.venue,
            "venue_address": event.venue_address,
            "timezone": event.timezone,
            "starts_at": event.starts_at,
            "ticket_url": event.ticket_url,
            "bandsintown_event_url": event.external_event_url,
        }),
    )
    .await?;
    sqlx::query(
        "UPDATE event_announcements SET regional_recipient_count = $3 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(source.workspace_id)
    .bind(announcement_id)
    .bind(recipient_count)
    .execute(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    Ok(())
}

async fn announce_event_change(
    transaction: &mut Transaction<'_, Postgres>,
    source: &EventSourceRow,
    kind: &str,
    event: &PersistedEventSnapshot,
    previous: Option<&PersistedEventSnapshot>,
) -> Result<(), EventSyncError> {
    let fingerprint = event_change_fingerprint(kind, event, previous);
    let announcement_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO event_announcements (workspace_id, event_id, kind, fingerprint)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(source.workspace_id)
    .bind(event.id)
    .bind(kind)
    .bind(&fingerprint)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    let Some(announcement_id) = announcement_id else {
        return Ok(());
    };

    let event_type = match kind {
        "updated" => "event.updated",
        "cancelled" => "event.cancelled",
        _ => return Err(EventSyncError::InvalidSource),
    };
    append_event_outbox(
        transaction,
        source.workspace_id,
        event_type,
        &format!("event:{}:{kind}:{fingerprint}", event.id),
        json!({
            "announcement_id": announcement_id,
            "change_kind": kind,
            "event": persisted_event_payload(event),
            "previous": previous.map(persisted_event_payload),
            "source": {
                "provider": source.provider,
                "artist_name": source.artist_name,
            },
        }),
    )
    .await?;

    let recipient_count = enqueue_event_change_outbox(
        transaction,
        source.workspace_id,
        announcement_id,
        event.id,
        kind,
        persisted_event_payload(event),
        previous.map(persisted_event_payload),
    )
    .await?;
    sqlx::query(
        "UPDATE event_announcements SET regional_recipient_count = $3 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(source.workspace_id)
    .bind(announcement_id)
    .bind(recipient_count)
    .execute(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    Ok(())
}

fn event_change_fingerprint(
    kind: &str,
    event: &PersistedEventSnapshot,
    previous: Option<&PersistedEventSnapshot>,
) -> String {
    let previous = previous.map(|value| {
        format!(
            "{}|{}|{}|{}|{}",
            value.city_id.map_or_else(String::new, |id| id.to_string()),
            value.starts_at.unix_timestamp_nanos(),
            value.venue.as_deref().unwrap_or_default(),
            value.venue_address.as_deref().unwrap_or_default(),
            value.status,
        )
    });
    let canonical = format!(
        "{kind}|{}|{}|{}|{}|{}|{}|{}",
        event.id,
        event.city_id.map_or_else(String::new, |id| id.to_string()),
        event.starts_at.unix_timestamp_nanos(),
        event.venue.as_deref().unwrap_or_default(),
        event.venue_address.as_deref().unwrap_or_default(),
        event.status,
        previous.as_deref().unwrap_or_default(),
    );
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

fn persisted_event_payload(event: &PersistedEventSnapshot) -> serde_json::Value {
    json!({
        "id": event.id,
        "slug": event.slug,
        "title": event.title,
        "description": event.description,
        "city": event.city_name,
        "country_code": event.country_code,
        "venue": event.venue,
        "venue_address": event.venue_address,
        "timezone": event.timezone,
        "starts_at": event.starts_at,
        "ticket_url": event.ticket_url,
        "bandsintown_event_url": event.external_event_url,
        "status": event.status,
    })
}

async fn enqueue_event_change_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    announcement_id: Uuid,
    event_id: Uuid,
    change_kind: &str,
    event_payload: serde_json::Value,
    previous_payload: Option<serde_json::Value>,
) -> Result<i32, EventSyncError> {
    let inserted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH candidates AS (
            SELECT
                fan.id,
                fan.normalized_email,
                fan.display_name,
                fan.locale,
                1 AS priority
            FROM event_interests AS interest
            JOIN fans AS fan
              ON fan.workspace_id = interest.workspace_id
             AND fan.id = interest.fan_id
            WHERE interest.workspace_id = $1
              AND interest.event_id = $3
              AND fan.status = 'active'

            UNION ALL

            SELECT
                ticket_order.id,
                lower(btrim(ticket_order.buyer_email)) AS normalized_email,
                ticket_order.buyer_name AS display_name,
                ticket_order.buyer_locale AS locale,
                2 AS priority
            FROM ticket_orders AS ticket_order
            JOIN ticket_sales AS sale
              ON sale.workspace_id = ticket_order.workspace_id
             AND sale.id = ticket_order.ticket_sale_id
            WHERE ticket_order.workspace_id = $1
              AND sale.event_id = $3
              AND ticket_order.status IN ('paid', 'partially_refunded')
        ), unique_recipients AS (
            SELECT DISTINCT ON (normalized_email)
                id, normalized_email, display_name, locale
            FROM candidates
            WHERE normalized_email <> ''
            ORDER BY normalized_email, priority, id
            LIMIT 10000
        ), inserted AS (
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id
            )
            SELECT
                $1,
                'event.change_due',
                1,
                jsonb_build_object(
                    'announcement_id', $2,
                    'change_kind', $4,
                    'fan', jsonb_build_object(
                        'id', recipient.id,
                        'email', recipient.normalized_email,
                        'display_name', recipient.display_name,
                        'locale', recipient.locale
                    ),
                    'event', $5::jsonb,
                    'previous', $6::jsonb
                ),
                'announcement:' || $2::text || ':fan:' || recipient.id::text
            FROM unique_recipients AS recipient
            RETURNING 1
        )
        SELECT count(*)::bigint FROM inserted
        "#,
    )
    .bind(workspace_id)
    .bind(announcement_id)
    .bind(event_id)
    .bind(change_kind)
    .bind(event_payload)
    .bind(previous_payload.unwrap_or(serde_json::Value::Null))
    .fetch_one(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    i32::try_from(inserted).map_err(|_| EventSyncError::Database)
}

async fn enqueue_regional_announcement_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    announcement_id: Uuid,
    city_id: Option<Uuid>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    event_payload: serde_json::Value,
) -> Result<i32, EventSyncError> {
    let inserted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH latest_marketing_consent AS (
            SELECT DISTINCT ON (consent.fan_id)
                consent.fan_id,
                consent.granted
            FROM fan_consents AS consent
            WHERE consent.workspace_id = $1
              AND consent.purpose = 'marketing'
            ORDER BY consent.fan_id, consent.recorded_at DESC, consent.id DESC
        ), candidates AS (
            SELECT DISTINCT ON (fan.normalized_email)
                fan.id,
                fan.normalized_email,
                fan.display_name,
                fan.locale
            FROM fans AS fan
            JOIN latest_marketing_consent AS consent
              ON consent.fan_id = fan.id
             AND consent.granted
            JOIN fan_city_interests AS interest
              ON interest.workspace_id = fan.workspace_id
             AND interest.fan_id = fan.id
            JOIN cities AS fan_city ON fan_city.id = interest.city_id
            WHERE fan.workspace_id = $1
              AND fan.status = 'active'
              AND (
                  ($3::uuid IS NOT NULL AND interest.city_id = $3)
                  OR (
                      $4::double precision IS NOT NULL
                      AND $5::double precision IS NOT NULL
                      AND fan_city.latitude IS NOT NULL
                      AND fan_city.longitude IS NOT NULL
                      AND 6371.0 * 2.0 * asin(least(1.0, greatest(0.0, sqrt(
                          power(sin(radians((fan_city.latitude - $4) / 2.0)), 2)
                          + cos(radians($4)) * cos(radians(fan_city.latitude))
                          * power(sin(radians((fan_city.longitude - $5) / 2.0)), 2)
                      )))) <= 150.0
                  )
              )
            ORDER BY fan.normalized_email, fan.id
            LIMIT 10000
        ), inserted AS (
            INSERT INTO outbox_events (
                workspace_id, event_type, event_version, payload, request_id, available_at
            )
            SELECT
                $1,
                'event.announcement_due',
                1,
                jsonb_build_object(
                    'announcement_id', $2,
                    'fan', jsonb_build_object(
                        'id', recipient.id,
                        'email', recipient.normalized_email,
                        'display_name', recipient.display_name,
                        'locale', recipient.locale
                    ),
                    'event', $6::jsonb
                ),
                'announcement:' || $2::text || ':fan:' || recipient.id::text,
                now() + interval '90 seconds'
            FROM candidates AS recipient
            RETURNING 1
        )
        SELECT count(*)::bigint FROM inserted
        "#,
    )
    .bind(workspace_id)
    .bind(announcement_id)
    .bind(city_id)
    .bind(latitude)
    .bind(longitude)
    .bind(event_payload)
    .fetch_one(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    i32::try_from(inserted).map_err(|_| EventSyncError::Database)
}

async fn append_delayed_event_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event_type: &str,
    request_id: &str,
    payload: serde_json::Value,
) -> Result<(), EventSyncError> {
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id, available_at
        )
        VALUES ($1, $2, 1, $3, $4, now() + interval '90 seconds')
        "#,
    )
    .bind(workspace_id)
    .bind(event_type)
    .bind(payload)
    .bind(request_id)
    .execute(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    Ok(())
}

async fn append_event_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event_type: &str,
    request_id: &str,
    payload: serde_json::Value,
) -> Result<(), EventSyncError> {
    sqlx::query(
        "INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id) VALUES ($1, $2, 1, $3, $4)",
    )
    .bind(workspace_id)
    .bind(event_type)
    .bind(payload)
    .bind(request_id)
    .execute(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    Ok(())
}
