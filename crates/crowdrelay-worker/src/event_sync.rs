//! Asynchronous event ingestion from external providers.
//!
//! The public site reads CrowdRelay's local event table only. This worker leases
//! due sources, performs the remote request outside a database transaction, then
//! atomically upserts the normalized result. A failed provider call never removes
//! or hides previously persisted concerts.

use std::{collections::HashSet, env, time::Duration};

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
    bandsintown_api_key: Option<String>,
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
        let bandsintown_api_key =
            normalize_provider_api_key(env::var("CROWDRELAY_BANDSINTOWN_API_KEY").ok())?;
        let client = Client::builder()
            .timeout(config.http_timeout)
            .connect_timeout(config.http_timeout.min(Duration::from_secs(5)))
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent("CrowdRelay/0.1 event-sync")
            .build()
            .map_err(|_| EventSyncError::InvalidConfiguration)?;
        Ok(Self {
            pool,
            client,
            config,
            bandsintown_api_key,
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
        let app_id =
            resolve_bandsintown_app_id(self.bandsintown_api_key.as_deref(), source.app_id.as_str());
        let mut url = Url::parse(&format!(
            "https://rest.bandsintown.com/artists/{}/events",
            encode_path_segment(&source.artist_name)
        ))
        .map_err(|_| EventSyncError::InvalidSource)?;
        url.query_pairs_mut()
            .append_pair("app_id", app_id)
            .append_pair("date", "upcoming");

        let response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| EventSyncError::ProviderUnavailable)?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(EventSyncError::ProviderAuthentication(
                response.status().as_u16(),
            ));
        }
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
    #[error("event source provider rejected the configured API key (HTTP {0})")]
    ProviderAuthentication(u16),
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

include!("event_sync/persistence.rs");
include!("event_sync/announcements.rs");
async fn record_source_failure(
    pool: &PgPool,
    source: &EventSourceRow,
    error: &EventSyncError,
    config: &EventSyncWorkerConfig,
) -> Result<(), EventSyncError> {
    let failures = source.consecutive_failures.saturating_add(1).min(16);
    let retry_seconds = retry_delay_seconds(source.sync_interval_seconds, failures)?;
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

fn retry_delay_seconds(sync_interval_seconds: i32, failures: i32) -> Result<i64, EventSyncError> {
    let interval = i64::from(sync_interval_seconds).max(60);
    let exponent = u32::try_from(failures.saturating_sub(1).clamp(0, 8))
        .map_err(|_| EventSyncError::InvalidSource)?;
    // Recover transient provider/network failures quickly (5/10/20 min) but
    // never poll more slowly than the configured healthy cadence.
    Ok(300_i64
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(interval))
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

fn normalize_provider_api_key(value: Option<String>) -> Result<Option<String>, EventSyncError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(EventSyncError::InvalidConfiguration);
    }
    Ok(Some(value.to_owned()))
}

fn resolve_bandsintown_app_id<'a>(
    configured_api_key: Option<&'a str>,
    source_app_id: &'a str,
) -> &'a str {
    configured_api_key.unwrap_or(source_app_id)
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

    #[test]
    fn bandsintown_api_key_override_is_trimmed_and_validated() {
        assert_eq!(
            normalize_provider_api_key(Some("  artist-key_123  ".to_owned()))
                .ok()
                .flatten()
                .as_deref(),
            Some("artist-key_123")
        );
        assert!(normalize_provider_api_key(Some("bad key".to_owned())).is_err());
        assert!(normalize_provider_api_key(Some(String::new())).is_err());
    }

    #[test]
    fn bandsintown_environment_key_overrides_stale_source_app_id() {
        assert_eq!(
            resolve_bandsintown_app_id(Some("environment-key"), "stale-database-app-id"),
            "environment-key"
        );
        assert_eq!(
            resolve_bandsintown_app_id(None, "legacy-database-app-id"),
            "legacy-database-app-id"
        );
    }
}

#[cfg(test)]
mod retry_policy_tests {
    use super::*;

    #[test]
    fn transient_event_sync_retries_before_normal_cadence() {
        assert_eq!(retry_delay_seconds(1800, 1).ok(), Some(300));
        assert_eq!(retry_delay_seconds(1800, 2).ok(), Some(600));
        assert_eq!(retry_delay_seconds(1800, 3).ok(), Some(1200));
        assert_eq!(retry_delay_seconds(1800, 4).ok(), Some(1800));
        assert_eq!(retry_delay_seconds(3600, 4).ok(), Some(2400));
        assert_eq!(retry_delay_seconds(300, 8).ok(), Some(300));
    }
}
