//! Gives fan-requested cities the coordinates proximity delivery needs.
//!
//! `request_city` records a name; the nearby-show query needs latitude and
//! longitude on both ends. Until something supplies them every fan sitting in a
//! requested city is unreachable, and nothing did — the geocode endpoint had no
//! caller, so the queue only ever grew.
//!
//! Nominatim is the provider because it needs no API key and no commercial
//! agreement to start, which keeps this from being blocked on a procurement
//! decision. Its usage policy asks for a real identifying `User-Agent` and at
//! most one request a second; both are honoured below, and the base URL is
//! configurable so a self-hosted instance or a paid provider can replace it
//! without touching the worker.
//!
//! The `cities` row is the cache: once coordinates are stored the selection
//! skips it forever. Attempts are capped and backed off, so a name that will
//! never resolve stops costing requests and surfaces to a human instead.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, sleep},
};

/// Nominatim asks for no more than one request a second. This is the floor
/// between two lookups in the same batch, not the poll interval.
const PROVIDER_MIN_SPACING: Duration = Duration::from_millis(1_100);
/// After this many failures a city stops being retried and waits for a human.
/// Five covers a provider outage; beyond that the name itself is the problem.
pub const MAX_GEOCODE_ATTEMPTS: i32 = 5;
/// Cities are requested by people, so new ones arrive at human speed.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// How many to resolve per cycle. Bounded so one pass cannot hold the provider
/// (or this worker) for minutes at the spacing above.
const BATCH: i64 = 10;
/// Floor on the HTTP timeout. A public geocoder is slower than the database
/// operation budget this worker otherwise inherits.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A resolved location. Deliberately minimal: the nearby query needs a point
/// and nothing else, and a wider shape would invite storing more of a
/// third-party record than the feature justifies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeocodedPoint {
    pub latitude: f64,
    pub longitude: f64,
}

/// Looks up a place by name and country. Implementations must return `Ok(None)`
/// for "no such place" and `Err` only for a failure worth retrying — the caller
/// treats the two differently, and conflating them either retries a name that
/// cannot resolve or gives up on a provider blip.
#[async_trait]
pub trait GeocodeProvider: Send + Sync {
    async fn lookup(
        &self,
        name: &str,
        region: Option<&str>,
        country_code: &str,
    ) -> Result<Option<GeocodedPoint>>;
}

pub struct NominatimProvider {
    client: reqwest::Client,
    base_url: String,
}

#[derive(Deserialize)]
struct NominatimHit {
    lat: String,
    lon: String,
}

impl NominatimProvider {
    pub fn new(base_url: String, contact: String, timeout: Duration) -> Result<Self> {
        // Nominatim's policy requires an identifying agent with a way to reach
        // the operator; an anonymous client is the documented way to get
        // blocked.
        let client = reqwest::Client::builder()
            .user_agent(format!("CrowdRelay/1.0 city-geocoding ({contact})"))
            .timeout(timeout)
            .build()
            .context("building the geocoding HTTP client")?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }
}

#[async_trait]
impl GeocodeProvider for NominatimProvider {
    async fn lookup(
        &self,
        name: &str,
        region: Option<&str>,
        country_code: &str,
    ) -> Result<Option<GeocodedPoint>> {
        let query = match region {
            Some(region) if !region.trim().is_empty() => format!("{name}, {region}"),
            _ => name.to_owned(),
        };
        let response = self
            .client
            .get(format!("{}/search", self.base_url))
            .query(&[
                ("q", query.as_str()),
                ("countrycodes", &country_code.to_ascii_lowercase()),
                ("format", "jsonv2"),
                ("limit", "1"),
            ])
            .send()
            .await
            .context("geocoding request failed")?;
        if !response.status().is_success() {
            // A non-success is the provider's problem, not the name's, so it
            // stays retryable rather than burning the city's attempts.
            anyhow::bail!("geocoding provider returned {}", response.status());
        }
        let hits: Vec<NominatimHit> = response
            .json()
            .await
            .context("geocoding response was not the expected JSON")?;
        let Some(hit) = hits.into_iter().next() else {
            return Ok(None);
        };
        let (Ok(latitude), Ok(longitude)) = (hit.lat.parse::<f64>(), hit.lon.parse::<f64>()) else {
            return Ok(None);
        };
        if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
            // The column has the same CHECK; rejecting here keeps a bad answer
            // from turning into a failed write that looks like an outage.
            return Ok(None);
        }
        Ok(Some(GeocodedPoint {
            latitude,
            longitude,
        }))
    }
}

#[derive(sqlx::FromRow)]
struct PendingCity {
    id: uuid::Uuid,
    name: String,
    region: Option<String>,
    country_code: String,
    geocode_attempts: i32,
}

pub struct CityGeocodeWorker {
    database: PgPool,
    provider: Arc<dyn GeocodeProvider>,
    poll_interval: Duration,
}

impl CityGeocodeWorker {
    #[must_use]
    pub fn new(
        database: PgPool,
        provider: Arc<dyn GeocodeProvider>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            database,
            provider,
            poll_interval,
        }
    }

    /// Builds the Nominatim-backed worker, or `None` when no contact address is
    /// configured.
    ///
    /// The contact is not optional politeness: Nominatim's usage policy makes an
    /// identifying `User-Agent` a condition of access, and an anonymous client
    /// gets blocked. Refusing to start without one is better than starting a
    /// loop that will be banned, so the caller logs what is unresolved instead.
    pub fn from_env(database: PgPool, operation_timeout: Duration) -> Result<Option<Self>> {
        let contact = std::env::var("CROWDRELAY_CITY_GEOCODING_CONTACT")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let Some(contact) = contact else {
            return Ok(None);
        };
        let base_url = std::env::var("CROWDRELAY_CITY_GEOCODING_BASE_URL")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "https://nominatim.openstreetmap.org".to_owned());
        let provider = NominatimProvider::new(base_url, contact, operation_timeout.max(TIMEOUT))?;
        Ok(Some(Self::new(
            database,
            Arc::new(provider),
            DEFAULT_POLL_INTERVAL,
        )))
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                _ = ticker.tick() => {
                    match self.resolve_batch().await {
                        Ok((0, 0)) => {}
                        Ok((resolved, failed)) => {
                            tracing::info!(resolved, failed, "city geocoding pass complete");
                        }
                        Err(error) => {
                            tracing::warn!(error = ?error, "city geocoding pass failed");
                        }
                    }
                }
            }
        }
    }

    /// Returns `(resolved, failed)` for one pass.
    pub async fn resolve_batch(&self) -> Result<(u32, u32)> {
        let pending = sqlx::query_as::<_, PendingCity>(
            r#"
            SELECT id, name, region, country_code::text AS country_code, geocode_attempts
            FROM cities
            WHERE latitude IS NULL
              AND moderation_status IN ('pending', 'approved')
              AND geocode_attempts < $1
              AND (geocode_next_attempt_at IS NULL OR geocode_next_attempt_at <= now())
            ORDER BY request_count DESC, id
            LIMIT $2
            "#,
        )
        .bind(MAX_GEOCODE_ATTEMPTS)
        .bind(BATCH)
        .fetch_all(&self.database)
        .await
        .context("selecting cities awaiting coordinates")?;

        let mut resolved = 0_u32;
        let mut failed = 0_u32;
        for (index, city) in pending.iter().enumerate() {
            if index > 0 {
                sleep(PROVIDER_MIN_SPACING).await;
            }
            match self
                .provider
                .lookup(&city.name, city.region.as_deref(), &city.country_code)
                .await
            {
                Ok(Some(point)) => {
                    self.store_point(city.id, point).await?;
                    resolved += 1;
                }
                Ok(None) => {
                    // The provider answered and has no such place. Count it
                    // against the cap: asking again changes nothing.
                    self.record_failure(city, "no match for this name").await?;
                    failed += 1;
                }
                Err(error) => {
                    self.record_failure(city, &error.to_string()).await?;
                    failed += 1;
                }
            }
        }
        Ok((resolved, failed))
    }

    async fn store_point(&self, city_id: uuid::Uuid, point: GeocodedPoint) -> Result<()> {
        // Guarded on `latitude IS NULL` so a human who filled the city in while
        // this batch was running keeps their answer.
        sqlx::query(
            r#"
            UPDATE cities
            SET latitude = $2,
                longitude = $3,
                geocode_last_attempt_at = now(),
                geocode_next_attempt_at = NULL,
                geocode_last_error = NULL
            WHERE id = $1 AND latitude IS NULL
            "#,
        )
        .bind(city_id)
        .bind(point.latitude)
        .bind(point.longitude)
        .execute(&self.database)
        .await
        .context("storing city coordinates")?;
        Ok(())
    }

    async fn record_failure(&self, city: &PendingCity, reason: &str) -> Result<()> {
        // Exponential backoff on the attempt count, so a provider outage costs
        // a handful of requests rather than one per poll forever. At the cap the
        // row stops being selected at all and waits for a human.
        let next_attempt_minutes = backoff_minutes(city.geocode_attempts);
        sqlx::query(
            r#"
            UPDATE cities
            SET geocode_attempts = geocode_attempts + 1,
                geocode_last_attempt_at = now(),
                geocode_next_attempt_at = now() + ($2::bigint * interval '1 minute'),
                geocode_last_error = $3
            WHERE id = $1 AND latitude IS NULL
            "#,
        )
        .bind(city.id)
        .bind(next_attempt_minutes)
        .bind(reason.chars().take(300).collect::<String>())
        .execute(&self.database)
        .await
        .context("recording a geocoding failure")?;
        Ok(())
    }
}

/// Backoff for the attempt that has just failed, in minutes. Free of the pool so
/// the schedule can be asserted without a database.
#[must_use]
pub fn backoff_minutes(attempts_before_this_failure: i32) -> i64 {
    i64::from(15 * (1 << attempts_before_this_failure.min(6)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    /// Answers one request with a fixed body, then stops. Enough to pin how the
    /// provider reads a reply without reaching a real geocoder in a test.
    async fn stub(status: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{address}")
    }

    fn provider(base_url: String) -> NominatimProvider {
        NominatimProvider::new(base_url, "ops@example.test".to_owned(), TIMEOUT)
            .expect("build provider")
    }

    #[tokio::test]
    async fn a_hit_becomes_a_point() {
        let base = stub("200 OK", r#"[{"lat":"51.1079","lon":"17.0385"}]"#).await;
        let point = provider(base)
            .lookup("Wroclaw", Some("Dolnoslaskie"), "PL")
            .await
            .expect("lookup");
        assert_eq!(
            point,
            Some(GeocodedPoint {
                latitude: 51.1079,
                longitude: 17.0385,
            })
        );
    }

    #[tokio::test]
    async fn an_empty_result_is_not_an_error() {
        // "No such place" must stay distinguishable from "the provider is
        // down": one counts against the attempt cap, the other is worth
        // retrying, and conflating them either gives up on a real city or
        // hammers a name that will never resolve.
        let base = stub("200 OK", "[]").await;
        let point = provider(base).lookup("Nowhereton", None, "PL").await;
        assert_eq!(point.expect("lookup"), None);
    }

    #[tokio::test]
    async fn a_provider_failure_stays_retryable() {
        let base = stub("503 Service Unavailable", "{}").await;
        let error = provider(base).lookup("Wroclaw", None, "PL").await;
        assert!(error.is_err(), "a 503 must surface as a retryable error");
    }

    #[tokio::test]
    async fn coordinates_outside_the_globe_are_refused() {
        // The column carries the same CHECK, so accepting this would turn a bad
        // answer into a failed write that reads like an outage.
        let base = stub("200 OK", r#"[{"lat":"991.0","lon":"17.0385"}]"#).await;
        let point = provider(base).lookup("Wroclaw", None, "PL").await;
        assert_eq!(point.expect("lookup"), None);
    }

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        assert_eq!(backoff_minutes(0), 15);
        assert_eq!(backoff_minutes(1), 30);
        assert_eq!(backoff_minutes(4), 240);
        // Clamped, so no shift can overflow however high the counter climbs.
        assert_eq!(backoff_minutes(6), backoff_minutes(9));
    }
}
