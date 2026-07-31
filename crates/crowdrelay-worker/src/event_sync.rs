//! Asynchronous event ingestion from external providers.
//!
//! The public site reads CrowdRelay's local event table only. This worker leases
//! due sources, performs the remote request outside a database transaction, then
//! atomically upserts the normalized result. A failed provider call never removes
//! or hides previously persisted concerts.

use std::{collections::HashSet, time::Duration};

use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const SOURCE_LEASE_SECONDS: i64 = 300;
const MAX_SOURCES_PER_TICK: usize = 8;
const MAX_PROVIDER_EVENTS: usize = 500;
const MAX_PROVIDER_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 500;

#[derive(Clone, Debug)]
pub struct EventSyncWorkerConfig {
    pub poll_interval: Duration,
    pub http_timeout: Duration,
    pub operation_timeout: Duration,
    pub lock_timeout: Duration,
}

impl EventSyncWorkerConfig {
    #[must_use]
    pub const fn with_database_timeouts(
        operation_timeout: Duration,
        lock_timeout: Duration,
    ) -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            http_timeout: DEFAULT_HTTP_TIMEOUT,
            operation_timeout,
            lock_timeout,
        }
    }
}

#[derive(Clone)]
pub struct EventSyncWorker {
    pool: PgPool,
    client: Client,
    config: EventSyncWorkerConfig,
}

impl EventSyncWorker {
    pub fn new(pool: PgPool, config: EventSyncWorkerConfig) -> Result<Self, EventSyncError> {
        if config.poll_interval.is_zero()
            || config.http_timeout.is_zero()
            || config.operation_timeout.is_zero()
            || config.lock_timeout.is_zero()
        {
            return Err(EventSyncError::InvalidConfiguration);
        }
        let client = Client::builder()
            .timeout(config.http_timeout)
            .user_agent("CrowdRelay/0.1 event-sync")
            .build()
            .map_err(|_| EventSyncError::InvalidConfiguration)?;
        Ok(Self {
            pool,
            client,
            config,
        })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) = self.process_due_sources().await {
                        tracing::error!(error = %error, "event source synchronization failed");
                    }
                }
            }
        }
    }

    async fn process_due_sources(&self) -> Result<(), EventSyncError> {
        for _ in 0..MAX_SOURCES_PER_TICK {
            let source = timeout(
                self.config.operation_timeout,
                claim_source(&self.pool, &self.config),
            )
            .await
            .map_err(|_| EventSyncError::TimedOut)??;
            let Some(source) = source else {
                break;
            };

            let sync_started_at = OffsetDateTime::now_utc();
            let result = match source.provider.as_str() {
                "bandsintown" => self.fetch_bandsintown(&source).await,
                _ => Err(EventSyncError::UnsupportedProvider),
            };

            match result {
                Ok(events) => {
                    timeout(
                        self.config.operation_timeout.saturating_mul(2),
                        persist_success(
                            &self.pool,
                            &self.config,
                            &source,
                            sync_started_at,
                            &events,
                        ),
                    )
                    .await
                    .map_err(|_| EventSyncError::TimedOut)??;
                    tracing::info!(
                        source_id = %source.id,
                        provider = %source.provider,
                        artist = %source.artist_name,
                        event_count = events.len(),
                        "event source synchronized"
                    );
                }
                Err(error) => {
                    record_source_failure(&self.pool, &source, &error, &self.config).await?;
                    tracing::warn!(
                        source_id = %source.id,
                        provider = %source.provider,
                        artist = %source.artist_name,
                        error = %error,
                        "event source synchronization deferred"
                    );
                }
            }
        }
        Ok(())
    }

    async fn fetch_bandsintown(
        &self,
        source: &EventSourceRow,
    ) -> Result<Vec<NormalizedExternalEvent>, EventSyncError> {
        let mut url = Url::parse(&format!(
            "https://rest.bandsintown.com/artists/{}/events",
            encode_path_segment(&source.artist_name)
        ))
        .map_err(|_| EventSyncError::InvalidSource)?;
        url.query_pairs_mut()
            .append_pair("app_id", &source.app_id)
            .append_pair("date", "upcoming");

        let response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| EventSyncError::ProviderUnavailable)?;
        if !response.status().is_success() {
            return Err(EventSyncError::ProviderStatus(response.status().as_u16()));
        }

        let body = read_limited_body(response).await?;
        let payload = serde_json::from_slice::<Vec<BandsintownEvent>>(&body)
            .map_err(|_| EventSyncError::InvalidProviderPayload)?;
        if payload.len() > MAX_PROVIDER_EVENTS {
            return Err(EventSyncError::ProviderPayloadTooLarge);
        }

        payload
            .into_iter()
            .map(|event| normalize_bandsintown_event(event, source))
            .collect()
    }
}

async fn read_limited_body(mut response: reqwest::Response) -> Result<Vec<u8>, EventSyncError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_PAYLOAD_BYTES as u64)
    {
        return Err(EventSyncError::ProviderPayloadTooLarge);
    }

    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(16 * 1024)
        .min(MAX_PROVIDER_PAYLOAD_BYTES);
    let mut body = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| EventSyncError::ProviderUnavailable)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_PAYLOAD_BYTES {
            return Err(EventSyncError::ProviderPayloadTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Debug, Error)]
pub enum EventSyncError {
    #[error("event sync configuration is invalid")]
    InvalidConfiguration,
    #[error("event source configuration is invalid")]
    InvalidSource,
    #[error("event source provider is unsupported")]
    UnsupportedProvider,
    #[error("event source request timed out")]
    TimedOut,
    #[error("event source database operation failed")]
    Database,
    #[error("event source provider is unavailable")]
    ProviderUnavailable,
    #[error("event source provider returned HTTP {0}")]
    ProviderStatus(u16),
    #[error("event source provider returned an invalid payload")]
    InvalidProviderPayload,
    #[error("event source provider returned too many events")]
    ProviderPayloadTooLarge,
}

impl EventSyncError {
    fn sqlx(_: sqlx::Error) -> Self {
        Self::Database
    }
}

#[derive(Clone, Debug, FromRow)]
struct EventSourceRow {
    id: Uuid,
    workspace_id: Uuid,
    provider: String,
    artist_name: String,
    app_id: String,
    default_country_code: String,
    timezone: String,
    sync_interval_seconds: i32,
    consecutive_failures: i32,
    consecutive_empty_syncs: i32,
    last_success_at: Option<OffsetDateTime>,
    sync_lease_owner: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct BandsintownEvent {
    id: serde_json::Value,
    url: Option<String>,
    datetime: String,
    title: Option<String>,
    description: Option<String>,
    lineup: Option<Vec<String>>,
    venue: BandsintownVenue,
    offers: Option<Vec<BandsintownOffer>>,
}

#[derive(Debug, Deserialize)]
struct BandsintownVenue {
    name: Option<String>,
    city: Option<String>,
    region: Option<String>,
    country: Option<String>,
    location: Option<String>,
    latitude: Option<String>,
    longitude: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BandsintownOffer {
    #[serde(rename = "type")]
    offer_type: Option<String>,
    url: Option<String>,
    status: Option<String>,
}

#[derive(Clone, Debug)]
struct NormalizedExternalEvent {
    source_event_id: String,
    slug: String,
    title: String,
    description: Option<String>,
    city_name: Option<String>,
    city_slug: Option<String>,
    country_code: String,
    region: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    venue: Option<String>,
    venue_address: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    ticket_url: Option<String>,
    external_event_url: Option<String>,
}

#[derive(Clone, Debug, FromRow)]
struct PersistedEventSnapshot {
    id: Uuid,
    city_id: Option<Uuid>,
    city_name: Option<String>,
    country_code: Option<String>,
    slug: String,
    title: String,
    description: Option<String>,
    venue: Option<String>,
    venue_address: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    ticket_url: Option<String>,
    external_event_url: Option<String>,
    status: String,
}

#[derive(Clone, Debug)]
struct EventUpsertResult {
    current: PersistedEventSnapshot,
    previous: Option<PersistedEventSnapshot>,
    inserted: bool,
}

#[derive(Debug, FromRow)]
struct InsertedEventRow {
    id: Uuid,
    inserted: bool,
}

fn normalize_bandsintown_event(
    event: BandsintownEvent,
    source: &EventSourceRow,
) -> Result<NormalizedExternalEvent, EventSyncError> {
    let source_event_id = external_id(&event.id).ok_or(EventSyncError::InvalidProviderPayload)?;
    let starts_at = OffsetDateTime::parse(&event.datetime, &Rfc3339)
        .map_err(|_| EventSyncError::InvalidProviderPayload)?;
    let city_name = clean_optional(event.venue.city);
    let country_code = country_code(event.venue.country.as_deref(), &source.default_country_code);
    let city_slug = city_name
        .as_deref()
        .map(|name| stable_slug(name, &country_code));
    let lineup = event.lineup.unwrap_or_default();
    let title = clean_optional(event.title)
        .or_else(|| {
            let names: Vec<_> = lineup
                .into_iter()
                .filter_map(|name| clean_optional(Some(name)))
                .collect();
            (!names.is_empty()).then(|| names.join(" · "))
        })
        .unwrap_or_else(|| match city_name.as_deref() {
            Some(city) => format!("{} live — {city}", source.artist_name),
            None => format!("{} live", source.artist_name),
        });
    let ticket_url = event
        .offers
        .unwrap_or_default()
        .into_iter()
        .find(|offer| {
            offer.url.is_some()
                && offer.status.as_deref() != Some("cancelled")
                && offer
                    .offer_type
                    .as_deref()
                    .is_none_or(|kind| kind.eq_ignore_ascii_case("tickets"))
        })
        .and_then(|offer| valid_http_url(offer.url));

    Ok(NormalizedExternalEvent {
        slug: format!(
            "gig-{}-{}",
            source.id.simple(),
            stable_slug(&source_event_id, "event")
        ),
        source_event_id,
        title,
        description: clean_optional(event.description),
        city_name,
        city_slug,
        country_code,
        region: clean_optional(event.venue.region),
        latitude: event
            .venue
            .latitude
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| (-90.0..=90.0).contains(value)),
        longitude: event
            .venue
            .longitude
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| (-180.0..=180.0).contains(value)),
        venue: clean_optional(event.venue.name),
        venue_address: clean_optional(event.venue.location),
        timezone: source.timezone.clone(),
        starts_at,
        ticket_url,
        external_event_url: valid_http_url(event.url),
    })
}

async fn claim_source(
    pool: &PgPool,
    config: &EventSyncWorkerConfig,
) -> Result<Option<EventSourceRow>, EventSyncError> {
    let mut transaction = pool.begin().await.map_err(EventSyncError::sqlx)?;
    configure_transaction(&mut transaction, config).await?;
    let mut row = sqlx::query_as::<_, EventSourceRow>(
        r#"
        SELECT
            id, workspace_id, provider, artist_name, app_id,
            default_country_code, timezone, sync_interval_seconds,
            consecutive_failures, consecutive_empty_syncs, last_success_at, sync_lease_owner
        FROM event_sources
        WHERE active
          AND next_sync_at <= now()
          AND (sync_lease_until IS NULL OR sync_lease_until <= now())
        ORDER BY next_sync_at, id
        FOR UPDATE SKIP LOCKED
        LIMIT 1
        "#,
    )
    .fetch_optional(&mut *transaction)
    .await
    .map_err(EventSyncError::sqlx)?;

    if let Some(source) = row.as_mut() {
        let lease_owner = Uuid::now_v7();
        sqlx::query(
            r#"
            UPDATE event_sources
            SET last_started_at = now(),
                sync_lease_until = now() + ($3::bigint * interval '1 second'),
                sync_lease_owner = $4
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(source.workspace_id)
        .bind(source.id)
        .bind(SOURCE_LEASE_SECONDS)
        .bind(lease_owner)
        .execute(&mut *transaction)
        .await
        .map_err(EventSyncError::sqlx)?;
        source.sync_lease_owner = Some(lease_owner);
    }
    transaction.commit().await.map_err(EventSyncError::sqlx)?;
    Ok(row)
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    config: &EventSyncWorkerConfig,
) -> Result<(), EventSyncError> {
    let statement_ms = duration_milliseconds(config.operation_timeout)?;
    let lock_ms = duration_milliseconds(config.lock_timeout)?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(EventSyncError::sqlx)?;
    Ok(())
}

fn duration_milliseconds(value: Duration) -> Result<u128, EventSyncError> {
    let value = value.as_millis();
    if value == 0 || value > 2_147_483_647_u128 {
        return Err(EventSyncError::InvalidConfiguration);
    }
    Ok(value)
}

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

async fn record_source_failure(
    pool: &PgPool,
    source: &EventSourceRow,
    error: &EventSyncError,
    config: &EventSyncWorkerConfig,
) -> Result<(), EventSyncError> {
    let failures = source.consecutive_failures.saturating_add(1).min(16);
    let exponent = u32::try_from(failures).map_err(|_| EventSyncError::InvalidSource)?;
    let exponential = 2_i64.saturating_pow(exponent).min(256);
    let retry_seconds = i64::from(source.sync_interval_seconds)
        .saturating_mul(exponential)
        .min(86_400);
    let message: String = error.to_string().chars().take(MAX_ERROR_CHARS).collect();
    timeout(
        config.operation_timeout,
        sqlx::query(
            r#"
            UPDATE event_sources
            SET sync_lease_until = NULL,
                sync_lease_owner = NULL,
                last_synced_at = now(),
                consecutive_failures = consecutive_failures + 1,
                last_error = $3,
                next_sync_at = now() + ($4::bigint * interval '1 second')
            WHERE workspace_id = $1 AND id = $2 AND sync_lease_owner = $5
            "#,
        )
        .bind(source.workspace_id)
        .bind(source.id)
        .bind(message)
        .bind(retry_seconds)
        .bind(
            source
                .sync_lease_owner
                .ok_or(EventSyncError::InvalidSource)?,
        )
        .execute(pool),
    )
    .await
    .map_err(|_| EventSyncError::TimedOut)?
    .map_err(EventSyncError::sqlx)?;
    Ok(())
}

fn external_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => clean_optional(Some(value.clone())),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn valid_http_url(value: Option<String>) -> Option<String> {
    let value = clean_optional(value)?;
    let url = Url::parse(&value).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(value)
}

fn country_code(country: Option<&str>, fallback: &str) -> String {
    let Some(value) = country.map(str::trim).filter(|value| !value.is_empty()) else {
        return fallback.to_owned();
    };
    if value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return value.to_ascii_uppercase();
    }

    let code = if value.eq_ignore_ascii_case("poland") || value.eq_ignore_ascii_case("polska") {
        "PL"
    } else if value.eq_ignore_ascii_case("germany") || value.eq_ignore_ascii_case("deutschland") {
        "DE"
    } else if value.eq_ignore_ascii_case("czech republic") || value.eq_ignore_ascii_case("czechia")
    {
        "CZ"
    } else if value.eq_ignore_ascii_case("slovakia") {
        "SK"
    } else if value.eq_ignore_ascii_case("austria") {
        "AT"
    } else if value.eq_ignore_ascii_case("hungary") {
        "HU"
    } else if value.eq_ignore_ascii_case("lithuania") {
        "LT"
    } else if value.eq_ignore_ascii_case("latvia") {
        "LV"
    } else if value.eq_ignore_ascii_case("estonia") {
        "EE"
    } else if value.eq_ignore_ascii_case("netherlands") {
        "NL"
    } else if value.eq_ignore_ascii_case("belgium") {
        "BE"
    } else if value.eq_ignore_ascii_case("france") {
        "FR"
    } else if value.eq_ignore_ascii_case("italy") {
        "IT"
    } else if value.eq_ignore_ascii_case("spain") {
        "ES"
    } else if value.eq_ignore_ascii_case("portugal") {
        "PT"
    } else if value.eq_ignore_ascii_case("sweden") {
        "SE"
    } else if value.eq_ignore_ascii_case("norway") {
        "NO"
    } else if value.eq_ignore_ascii_case("denmark") {
        "DK"
    } else if value.eq_ignore_ascii_case("finland") {
        "FI"
    } else if value.eq_ignore_ascii_case("ireland") {
        "IE"
    } else if value.eq_ignore_ascii_case("united kingdom") || value.eq_ignore_ascii_case("uk") {
        "GB"
    } else if value.eq_ignore_ascii_case("united states") || value.eq_ignore_ascii_case("usa") {
        "US"
    } else if value.eq_ignore_ascii_case("canada") {
        "CA"
    } else {
        fallback
    };
    code.to_owned()
}

fn stable_slug(value: &str, fallback: &str) -> String {
    let mut slug = String::with_capacity(value.len());
    let mut previous_dash = false;
    for character in value.chars().map(ascii_fold) {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str(fallback);
    }
    if slug.len() > 80 {
        slug.truncate(67);
        while slug.ends_with('-') {
            slug.pop();
        }
        let hash = Sha256::digest(value.as_bytes());
        let suffix = hash
            .get(..6)
            .map(hex::encode)
            .unwrap_or_else(|| "000000000000".to_owned());
        slug.push('-');
        slug.push_str(&suffix);
    }
    slug
}

fn ascii_fold(character: char) -> char {
    match character {
        'ą' | 'Ą' => 'a',
        'ć' | 'Ć' | 'č' | 'Č' => 'c',
        'ď' | 'Ď' => 'd',
        'ę' | 'Ę' | 'é' | 'É' | 'ě' | 'Ě' => 'e',
        'í' | 'Í' => 'i',
        'ł' | 'Ł' => 'l',
        'ń' | 'Ń' | 'ň' | 'Ň' => 'n',
        'ó' | 'Ó' | 'ö' | 'Ö' => 'o',
        'ř' | 'Ř' => 'r',
        'ś' | 'Ś' | 'š' | 'Š' => 's',
        'ť' | 'Ť' => 't',
        'ü' | 'Ü' | 'ú' | 'Ú' | 'ů' | 'Ů' => 'u',
        'ý' | 'Ý' => 'y',
        'ź' | 'Ź' | 'ż' | 'Ż' | 'ž' | 'Ž' => 'z',
        'ä' | 'Ä' | 'á' | 'Á' | 'à' | 'À' | 'â' | 'Â' => 'a',
        other => other,
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(*byte))
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_snapshot() -> PersistedEventSnapshot {
        PersistedEventSnapshot {
            id: Uuid::nil(),
            city_id: None,
            city_name: Some("Wrocław".to_owned()),
            country_code: Some("PL".to_owned()),
            slug: "gig-test".to_owned(),
            title: "Virya live".to_owned(),
            description: None,
            venue: Some("Klub".to_owned()),
            venue_address: Some("Rynek 1".to_owned()),
            timezone: "Europe/Warsaw".to_owned(),
            starts_at: OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("test timestamp should be valid"),
            ticket_url: None,
            external_event_url: None,
            status: "published".to_owned(),
        }
    }

    #[test]
    fn slug_is_bounded_and_stable() {
        let value = "Łódź / VIRYA live".repeat(30);
        let first = stable_slug(&value, "event");
        assert_eq!(first, stable_slug(&value, "event"));
        assert!(first.len() <= 127);
        assert!(first.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        }));
        assert_eq!(stable_slug("Łódź", "city"), "lodz");
    }

    #[test]
    fn path_segment_is_percent_encoded() {
        assert_eq!(encode_path_segment("Virya PL"), "Virya%20PL");
    }

    #[test]
    fn only_new_future_events_after_the_initial_sync_are_announced() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("test timestamp should be valid");
        assert!(!should_announce_new_event(
            false,
            true,
            now + time::Duration::days(1),
            now,
        ));
        assert!(!should_announce_new_event(
            true,
            false,
            now + time::Duration::days(1),
            now,
        ));
        assert!(!should_announce_new_event(true, true, now, now,));
        assert!(should_announce_new_event(
            true,
            true,
            now + time::Duration::days(1),
            now,
        ));
    }

    #[test]
    fn only_operational_event_changes_trigger_change_notifications() {
        let previous = event_snapshot();
        let mut current = previous.clone();
        current.title = "Updated promotional title".to_owned();
        assert!(!meaningful_event_change(&previous, &current));

        current.venue = Some("Nowy klub".to_owned());
        assert!(meaningful_event_change(&previous, &current));

        let mut current = previous.clone();
        current.starts_at = previous.starts_at + time::Duration::hours(1);
        assert!(meaningful_event_change(&previous, &current));

        let mut current = previous.clone();
        current.status = "cancelled".to_owned();
        assert!(meaningful_event_change(&previous, &current));
    }

    #[test]
    fn known_country_names_are_normalized() {
        assert_eq!(country_code(Some("Poland"), "XX"), "PL");
        assert_eq!(country_code(Some("Czechia"), "XX"), "CZ");
        assert_eq!(country_code(Some("Unknown"), "PL"), "PL");
    }
}
