#[derive(FromRow)]
struct PublicEventRow {
    id: Uuid,
    slug: String,
    title: String,
    description: Option<String>,
    city_id: Option<Uuid>,
    city_slug: Option<String>,
    city_name: Option<String>,
    country_code: Option<String>,
    region: Option<String>,
    venue: Option<String>,
    venue_address: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
    ticket_url: Option<String>,
    listen_url: Option<String>,
    image_url: Option<String>,
    trailer_url: Option<String>,
    external_event_url: Option<String>,
    updated_at: OffsetDateTime,
}

impl TryFrom<PublicEventRow> for PublicEvent {
    type Error = EventStoreError;

    fn try_from(row: PublicEventRow) -> Result<Self, Self::Error> {
        let city = match (row.city_id, row.city_slug, row.city_name, row.country_code) {
            (Some(id), Some(slug), Some(name), Some(country_code)) => Some(EventCity {
                id: CityId::from_uuid(id),
                slug,
                name,
                country_code,
                region: row.region,
            }),
            (None, None, None, None) => None,
            _ => return Err(EventStoreError::Unexpected),
        };
        let event = PublicEvent {
            id: EventId::from_uuid(row.id),
            slug: EventSlug::parse(row.slug).map_err(|_| EventStoreError::Unexpected)?,
            title: row.title,
            description: row.description,
            city,
            venue: row.venue,
            venue_address: row.venue_address,
            timezone: row.timezone,
            starts_at: row.starts_at,
            doors_at: row.doors_at,
            ends_at: row.ends_at,
            ticket_url: row.ticket_url,
            listen_url: row.listen_url,
            image_url: row.image_url,
            trailer_url: row.trailer_url,
            external_event_url: row.external_event_url,
            updated_at: row.updated_at,
        };
        event.validate().map_err(|_| EventStoreError::Unexpected)?;
        Ok(event)
    }
}

#[derive(FromRow)]
struct FanInterestRow {
    id: Uuid,
    slug: String,
    title: String,
    description: Option<String>,
    city_id: Option<Uuid>,
    city_slug: Option<String>,
    city_name: Option<String>,
    country_code: Option<String>,
    region: Option<String>,
    venue: Option<String>,
    venue_address: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
    ticket_url: Option<String>,
    listen_url: Option<String>,
    image_url: Option<String>,
    trailer_url: Option<String>,
    external_event_url: Option<String>,
    updated_at: OffsetDateTime,
    interested_at: OffsetDateTime,
}

impl TryFrom<FanInterestRow> for FanEventInterest {
    type Error = EventStoreError;
    fn try_from(row: FanInterestRow) -> Result<Self, Self::Error> {
        let event = PublicEvent::try_from(PublicEventRow {
            id: row.id,
            slug: row.slug,
            title: row.title,
            description: row.description,
            city_id: row.city_id,
            city_slug: row.city_slug,
            city_name: row.city_name,
            country_code: row.country_code,
            region: row.region,
            venue: row.venue,
            venue_address: row.venue_address,
            timezone: row.timezone,
            starts_at: row.starts_at,
            doors_at: row.doors_at,
            ends_at: row.ends_at,
            ticket_url: row.ticket_url,
            listen_url: row.listen_url,
            image_url: row.image_url,
            trailer_url: row.trailer_url,
            external_event_url: row.external_event_url,
            updated_at: row.updated_at,
        })?;
        Ok(Self {
            event,
            interested_at: row.interested_at,
        })
    }
}

#[derive(FromRow)]
struct InterestEventRow {
    id: Uuid,
    starts_at: OffsetDateTime,
}

#[derive(FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    state: String,
    response_body: Option<String>,
    lease_expired: bool,
}

#[derive(Serialize)]
struct InterestFingerprint<'a> {
    workspace_id: WorkspaceId,
    event_slug: &'a str,
    campaign_id: Option<CampaignId>,
    visitor_id: Option<VisitorId>,
    source: &'a str,
}

fn interest_request_hash(
    command: &RegisterEventInterestCommand,
) -> Result<Vec<u8>, EventStoreError> {
    let fingerprint = InterestFingerprint {
        workspace_id: command.workspace_id(),
        event_slug: command.event_slug().as_str(),
        campaign_id: command.campaign_id(),
        visitor_id: command.visitor_id(),
        source: command.source(),
    };
    let encoded = serde_json::to_vec(&fingerprint).map_err(|_| EventStoreError::Unexpected)?;
    Ok(Sha256::digest(encoded).to_vec())
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    operation_timeout: Duration,
    lock_timeout: Duration,
) -> Result<(), EventStoreError> {
    let statement_ms = duration_as_milliseconds(operation_timeout)?;
    let lock_ms = duration_as_milliseconds(lock_timeout)?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    Ok(())
}

async fn trusted_workspace_id_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_slug: &WorkspaceSlug,
) -> Result<WorkspaceId, EventStoreError> {
    let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
        .bind(workspace_slug.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(EventStoreError::from_sqlx)?
        .ok_or(EventStoreError::NotFound)?;
    Ok(WorkspaceId::from_uuid(id))
}

async fn current_referral_code_id(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: Uuid,
) -> Result<Option<Uuid>, EventStoreError> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT referral_code_id
        FROM fan_acquisition_events
        WHERE workspace_id = $1 AND fan_id = $2 AND referral_code_id IS NOT NULL
        ORDER BY occurred_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)
}

async fn schedule_reminders(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_id: EventId,
    fan_id: FanId,
    starts_at: OffsetDateTime,
    offsets: &[u32],
) -> Result<u32, EventStoreError> {
    let mut inserted = 0_u32;
    for offset_minutes in offsets {
        let offset = time::Duration::minutes(i64::from(*offset_minutes));
        let due_at = starts_at
            .checked_sub(offset)
            .ok_or(EventStoreError::Unexpected)?;
        if due_at <= OffsetDateTime::now_utc() {
            continue;
        }
        let kind = format!("{}m_before", offset_minutes);
        let result = sqlx::query(
            r#"
            INSERT INTO event_reminder_jobs (
                workspace_id, event_id, fan_id, reminder_kind, due_at
            ) VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (workspace_id, event_id, fan_id, reminder_kind) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(kind)
        .bind(due_at)
        .execute(&mut **transaction)
        .await
        .map_err(EventStoreError::from_sqlx)?;
        inserted = inserted.saturating_add(u32::try_from(result.rows_affected()).unwrap_or(0));
    }
    Ok(inserted)
}

async fn append_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_type: &str,
    request_id: &str,
    payload: serde_json::Value,
) -> Result<(), EventStoreError> {
    sqlx::query(
        r#"
        INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
        VALUES ($1, $2, 1, $3, $4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_type)
    .bind(payload)
    .bind(request_id)
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    Ok(())
}

async fn start_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RegisterEventInterestCommand,
    request_hash: &[u8],
    operation_timeout: Duration,
) -> Result<bool, EventStoreError> {
    let lease_ms = duration_as_milliseconds(operation_timeout)?;
    sqlx::query(
        r#"
        DELETE FROM idempotency_keys
        WHERE workspace_id = $1
            AND scope = $2
            AND key = $3
            AND expires_at <= now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;

    let result = sqlx::query(
        r#"
        INSERT INTO idempotency_keys (
            workspace_id, scope, key, request_hash, state,
            lease_owner, lease_expires_at, expires_at
        ) VALUES (
            $1, $2, $3, $4, 'in_progress', $5,
            now() + ($6::bigint * interval '1 millisecond'),
            now() + ($7::bigint * interval '1 millisecond')
        ) ON CONFLICT (workspace_id, scope, key) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(request_hash)
    .bind(command.request_id().as_str())
    .bind(lease_ms)
    .bind(IDEMPOTENCY_RETENTION_MILLISECONDS)
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    Ok(result.rows_affected() == 1)
}

async fn lock_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RegisterEventInterestCommand,
) -> Result<IdempotencyRow, EventStoreError> {
    sqlx::query_as::<_, IdempotencyRow>(
        r#"
        SELECT request_hash, state, response_body::text AS response_body,
               coalesce(lease_expires_at <= now(), false) AS lease_expired
        FROM idempotency_keys
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)
}

async fn reclaim_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RegisterEventInterestCommand,
    operation_timeout: Duration,
) -> Result<(), EventStoreError> {
    let lease_ms = duration_as_milliseconds(operation_timeout)?;
    let result = sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET lease_owner = $4,
            lease_expires_at = now() + ($5::bigint * interval '1 millisecond')
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
            AND state = 'in_progress' AND lease_expires_at <= now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(command.request_id().as_str())
    .bind(lease_ms)
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    if result.rows_affected() != 1 {
        return Err(EventStoreError::Conflict);
    }
    Ok(())
}

async fn complete_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RegisterEventInterestCommand,
    request_hash: &[u8],
    result: &EventInterestResult,
) -> Result<(), EventStoreError> {
    let response = serde_json::to_value(result).map_err(|_| EventStoreError::Unexpected)?;
    let updated = sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET state = 'completed', lease_owner = NULL, lease_expires_at = NULL,
            response_status = 200, response_body = $5,
            response_content_type = 'application/json', completed_at = now()
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
            AND request_hash = $4 AND state = 'in_progress'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(INTEREST_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(request_hash)
    .bind(response)
    .execute(&mut **transaction)
    .await
    .map_err(EventStoreError::from_sqlx)?;
    if updated.rows_affected() != 1 {
        return Err(EventStoreError::Conflict);
    }
    Ok(())
}

fn duration_as_milliseconds(duration: Duration) -> Result<i64, EventStoreError> {
    i64::try_from(duration.as_millis()).map_err(|_| EventStoreError::Unexpected)
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
enum EventStoreError {
    #[error("event store is unavailable")]
    Unavailable,
    #[error("event resource was not found")]
    NotFound,
    #[error("event request conflicts with durable state")]
    Conflict,
    #[error("event store failed unexpectedly")]
    Unexpected,
}

impl EventStoreError {
    fn from_sqlx(error: sqlx::Error) -> Self {
        match classify_sqlx_error(&error) {
            SqlxErrorClass::Unavailable => Self::Unavailable,
            SqlxErrorClass::NotFound => Self::NotFound,
            SqlxErrorClass::Conflict => Self::Conflict,
            SqlxErrorClass::Unexpected => Self::Unexpected,
        }
    }
}

impl From<EventStoreError> for RepositoryError {
    fn from(value: EventStoreError) -> Self {
        match value {
            EventStoreError::Unavailable => Self::Unavailable,
            EventStoreError::NotFound => Self::NotFound,
            EventStoreError::Conflict => Self::Conflict,
            EventStoreError::Unexpected => Self::Unexpected,
        }
    }
}
