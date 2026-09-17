//! Mints real rooms from OpenStreetMap in the cities tenants care about
//! (§12-3, 4V.8).
//!
//! The venue registry only knows rooms a tenant has played or a researcher
//! has typed into a sheet. OSM is the layer underneath both: it answers "does
//! this room exist, and where" for every city where somebody has fans or a
//! show, and turns "a name somebody typed" into a place with coordinates.
//!
//! Two constraints are licence terms, not preferences, and they shape the
//! code rather than a comment. ODbL is share-alike with required attribution
//! — every fact this sweep writes carries `licence = 'odbl'` on the row and
//! the sweep logs `osm-attribution` once per cycle. And the sweep never
//! writes a booking contact: an OSM `email`/`contact:*` tag is the room's
//! general inbox, and `BookingRefusalReason::RouteInferred` exists precisely
//! to refuse a route that was guessed rather than published. Contact-shaped
//! tags are dropped on the floor before a fact row is even built.
//!
//! Overpass is a shared public resource, so the politeness is structural: one
//! query per city, a 1.1 s floor between cities, a 24 h cycle with jitter so
//! deployments do not hit the endpoint in lockstep. The whole worker is
//! flag-gated (`CROWDRELAY_OSM_VENUE_SWEEP_ENABLED`, default off) until its
//! output has been verified in production.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use crowdrelay_infra::{
    venue_directory::PostgresVenueDirectoryRepository,
    venue_seed::{PostgresVenueSeedRepository, VenueFactWrite},
};
use getrandom::fill as fill_random;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::{sync::watch, time::sleep};
use uuid::Uuid;

/// Overpass asks its callers for about one request a second. This is the
/// floor between two cities, not the sweep cadence.
const PROVIDER_MIN_SPACING: Duration = Duration::from_millis(1_100);
/// The map does not change faster than daily for the cities that matter.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Extra delay drawn per cycle so a fleet of deployments does not arrive at
/// Overpass in lockstep.
const SWEEP_JITTER_MAX: Duration = Duration::from_secs(60 * 60);
/// The first cycle after boot waits only this much — a flag just turned on
/// should produce rows within minutes, not a day later.
const STARTUP_JITTER_MAX: Duration = Duration::from_secs(5 * 60);
/// A city's bbox reaches this far past its pin in each direction — roughly
/// 16 km of latitude, which covers the commute a city's rooms actually sit in.
const BBOX_HALF_DEGREES: f64 = 0.15;
/// The query carries its own `[timeout:60]`; the HTTP timeout leaves headroom
/// for a queued answer rather than racing it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(90);
/// One city's venue answer is small. A multi-megabyte reply is a runaway
/// query's output, not data, and is refused before it is parsed.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// The public Overpass endpoint. `CROWDRELAY_OSM_OVERPASS_URL` points the
/// worker at a mirror or a self-hosted instance without a rebuild.
const DEFAULT_OVERPASS_ENDPOINT: &str = "https://overpass-api.de/api/interpreter";

/// A rectangular window on the map in the order Overpass wants it:
/// south, west, north, east.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bbox {
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

impl Bbox {
    /// `±BBOX_HALF_DEGREES` around a city pin, clamped at the poles and the
    /// antimeridian so a city's position can never produce an invalid window.
    #[must_use]
    pub fn around(latitude: f64, longitude: f64) -> Self {
        Self {
            south: (latitude - BBOX_HALF_DEGREES).max(-90.0),
            west: (longitude - BBOX_HALF_DEGREES).max(-180.0),
            north: (latitude + BBOX_HALF_DEGREES).min(90.0),
            east: (longitude + BBOX_HALF_DEGREES).min(180.0),
        }
    }

    /// Four decimals is ~11 m — finer than any room's footprint and cleaner
    /// than f64's own rendering, which turns `17.0385 + 0.15` into dust.
    fn overpass(self) -> String {
        format!(
            "{:.4},{:.4},{:.4},{:.4}",
            self.south, self.west, self.north, self.east
        )
    }
}

/// One element of an Overpass `out center tags` reply. Nodes carry `lat`/`lon`
/// directly; ways carry a `center`.
#[derive(Clone, Debug, Deserialize)]
pub struct OsmElement {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: u64,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub center: Option<OsmCenter>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct OsmCenter {
    pub lat: f64,
    pub lon: f64,
}

impl OsmElement {
    /// The element's pin — a node's own coordinates, or a way's center.
    #[must_use]
    pub fn coordinates(&self) -> Option<(f64, f64)> {
        self.lat
            .zip(self.lon)
            .or_else(|| self.center.map(|center| (center.lat, center.lon)))
    }

    /// Which identifier scheme this element anchors under. Only node and way
    /// are queried, so a `relation` (or a parser surprise) resolves nothing.
    fn scheme(&self) -> Option<&'static str> {
        match self.kind.as_str() {
            "node" => Some("osm_node"),
            "way" => Some("osm_way"),
            _ => None,
        }
    }
}

/// What one element's ingest did. `Skipped` covers nameless elements and
/// element kinds the sweep does not anchor — counted, never written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestOutcome {
    Written,
    Skipped,
}

/// One cycle's tally, for the completion log.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SweepSummary {
    pub cities: u32,
    pub city_failures: u32,
    pub written: u32,
    pub skipped: u32,
    pub failed: u32,
}

/// The Overpass call itself, behind a trait so a test can answer from a
/// canned body rather than the public endpoint. `Err` means "the provider
/// failed", never "the city has no venues" — an empty element list is
/// `Ok(vec![])` and a failed city must not abort the cycle.
#[async_trait]
pub trait OverpassProvider: Send + Sync {
    async fn elements_in(&self, bbox: Bbox) -> Result<Vec<OsmElement>>;
}

/// The query every city gets: the four venue-ish shapes §12-3 names, inside
/// the city's bbox, with centers so a way still gives a pin.
fn overpass_query(bbox: Bbox) -> String {
    let bbox = bbox.overpass();
    format!(
        "[out:json][timeout:60];\n\
         (\n\
         \x20 node({bbox})[\"amenity\"=\"nightclub\"];\n\
         \x20 node({bbox})[\"amenity\"=\"music_venue\"];\n\
         \x20 way({bbox})[\"amenity\"=\"concert_hall\"];\n\
         \x20 node({bbox})[\"amenity\"=\"bar\"][\"live_music\"=\"yes\"];\n\
         );\n\
         out center tags;\n"
    )
}

/// The live provider: one POST per city to the configured Overpass endpoint.
pub struct OverpassClient {
    client: reqwest::Client,
    endpoint: String,
}

impl OverpassClient {
    /// An identifying `User-Agent` is how a shared public endpoint knows who
    /// is asking — an anonymous client is the documented way to get banned.
    pub fn new(endpoint: String, http_timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("CrowdRelay/1.0 osm-venue-sweep (venue map layer)")
            .timeout(http_timeout)
            .connect_timeout(http_timeout.min(Duration::from_secs(10)))
            .build()
            .context("building the Overpass HTTP client")?;
        Ok(Self { client, endpoint })
    }
}

#[async_trait]
impl OverpassProvider for OverpassClient {
    async fn elements_in(&self, bbox: Bbox) -> Result<Vec<OsmElement>> {
        let response = self
            .client
            .post(&self.endpoint)
            .form(&[("data", overpass_query(bbox))])
            .send()
            .await
            .context("overpass request failed")?;
        if !response.status().is_success() {
            bail!("overpass returned HTTP {}", response.status());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            bail!("overpass answer exceeds {MAX_RESPONSE_BYTES} bytes");
        }
        let body = response
            .bytes()
            .await
            .context("reading the overpass answer failed")?;
        if body.len() > MAX_RESPONSE_BYTES {
            bail!("overpass answer exceeds {MAX_RESPONSE_BYTES} bytes");
        }
        parse_overpass_body(&body)
    }
}

/// Reads one Overpass JSON body into elements. Separated from the transport
/// so the wire format is testable without a network: the tests feed it a
/// canned reply.
pub fn parse_overpass_body(body: &[u8]) -> Result<Vec<OsmElement>> {
    #[derive(Deserialize)]
    struct OverpassBody {
        #[serde(default)]
        elements: Vec<OsmElement>,
    }
    let parsed = serde_json::from_slice::<OverpassBody>(body)
        .context("overpass answer was not the expected JSON")?;
    Ok(parsed.elements)
}

/// The city the sweep is visiting — id for the venue's `city_id`, coordinates
/// for the bbox, the rest for logs a human can read.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct SweepCity {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub country_code: String,
    pub latitude: f64,
    pub longitude: f64,
}

pub struct OsmVenueSweepWorker {
    database: PgPool,
    provider: Arc<dyn OverpassProvider>,
    directory: PostgresVenueDirectoryRepository,
    sweep_interval: Duration,
    operation_timeout: Duration,
}

impl OsmVenueSweepWorker {
    #[must_use]
    pub fn new(
        database: PgPool,
        provider: Arc<dyn OverpassProvider>,
        sweep_interval: Duration,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            directory: PostgresVenueDirectoryRepository::new(database.clone()),
            database,
            provider,
            sweep_interval,
            operation_timeout,
        }
    }

    /// The production wiring: the public Overpass endpoint, or the mirror
    /// `CROWDRELAY_OSM_OVERPASS_URL` names. Returns `Err` only when the HTTP
    /// client cannot be built.
    pub fn standard(database: PgPool, operation_timeout: Duration) -> Result<Self> {
        let endpoint = std::env::var("CROWDRELAY_OSM_OVERPASS_URL")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_OVERPASS_ENDPOINT.to_owned());
        let provider = OverpassClient::new(endpoint, operation_timeout.max(HTTP_TIMEOUT))?;
        Ok(Self::new(
            database,
            Arc::new(provider),
            DEFAULT_SWEEP_INTERVAL,
            operation_timeout,
        ))
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut first_cycle = true;
        loop {
            // A plain `interval` would put every deployment on the same
            // phase; drawing the wait instead keeps the jitter inside the
            // select, where shutdown still answers immediately.
            let delay = if first_cycle {
                jitter(STARTUP_JITTER_MAX)
            } else {
                self.sweep_interval + jitter(SWEEP_JITTER_MAX)
            };
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = sleep(delay) => {}
            }
            first_cycle = false;
            if let Err(error) = self.sweep_once().await {
                tracing::warn!(%error, "OSM venue sweep cycle failed");
            }
        }
    }

    /// One cycle: the attribution line (the ODbL term, said once rather than
    /// per element), the city selection, then one Overpass call per city at
    /// the provider's spacing. A failed city is logged and left for the next
    /// cycle — it never cancels the rest of the sweep.
    pub async fn sweep_once(&self) -> Result<SweepSummary> {
        // ODbL attribution is a licence term: the statement names the source
        // and the licence in the log of every cycle that produced rows.
        tracing::info!(
            "osm-attribution: venue data © OpenStreetMap contributors, licensed under ODbL"
        );
        let cities = tokio::time::timeout(self.operation_timeout, self.sweep_cities())
            .await
            .context("selecting cities for the OSM sweep timed out")??;
        let mut summary = SweepSummary::default();
        for (index, city) in cities.iter().enumerate() {
            if index > 0 {
                sleep(PROVIDER_MIN_SPACING).await;
            }
            match self
                .provider
                .elements_in(Bbox::around(city.latitude, city.longitude))
                .await
            {
                Ok(elements) => {
                    summary.cities += 1;
                    for element in &elements {
                        // The database budget bounds the write, same as every
                        // other worker: a stalled pool must not pin the sweep.
                        match tokio::time::timeout(
                            self.operation_timeout,
                            self.ingest_element(city, element),
                        )
                        .await
                        {
                            Ok(Ok(IngestOutcome::Written)) => summary.written += 1,
                            Ok(Ok(IngestOutcome::Skipped)) => summary.skipped += 1,
                            Ok(Err(error)) => {
                                summary.failed += 1;
                                tracing::warn!(
                                    city = %city.slug,
                                    osm_kind = %element.kind,
                                    osm_id = element.id,
                                    %error,
                                    "OSM element ingest failed"
                                );
                            }
                            Err(_) => {
                                summary.failed += 1;
                                tracing::warn!(
                                    city = %city.slug,
                                    osm_kind = %element.kind,
                                    osm_id = element.id,
                                    "OSM element ingest timed out"
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    summary.city_failures += 1;
                    tracing::warn!(city = %city.slug, %error, "overpass query failed for city");
                }
            }
        }
        tracing::info!(
            cities = summary.cities,
            city_failures = summary.city_failures,
            venues_written = summary.written,
            elements_skipped = summary.skipped,
            elements_failed = summary.failed,
            "OSM venue sweep complete"
        );
        Ok(summary)
    }

    /// The cities worth a query: anywhere any tenant has a reachable fan or
    /// a show. "Has fans" is the nearby-gig definition — an active fan whose
    /// location preference is set in this city — the same source
    /// `place_reach` counts from. A city without coordinates is skipped by
    /// the WHERE itself: no pin, no bbox.
    async fn sweep_cities(&self) -> Result<Vec<SweepCity>> {
        sqlx::query_as::<_, SweepCity>(
            r#"
            SELECT DISTINCT c.id, c.slug, c.name, c.country_code::text AS country_code,
                   c.latitude, c.longitude
            FROM cities AS c
            WHERE c.latitude IS NOT NULL
              AND c.longitude IS NOT NULL
              AND (
                  EXISTS (
                      SELECT 1
                      FROM fan_location_preferences AS prefs
                      JOIN fans AS fan
                        ON fan.workspace_id = prefs.workspace_id
                       AND fan.id = prefs.fan_id
                      WHERE prefs.city_id = c.id
                        AND prefs.nearby_gigs_enabled
                        AND fan.status = 'active'
                  )
                  OR EXISTS (
                      SELECT 1
                      FROM events AS event
                      WHERE event.city_id = c.id
                        AND event.status IN ('published', 'completed')
                  )
              )
            ORDER BY c.id
            "#,
        )
        .fetch_all(&self.database)
        .await
        .context("selecting cities for the OSM sweep")
    }

    /// One element: anchor first, then mint, then facts.
    ///
    /// A known `(scheme, id)` anchor wins outright — that is the resolution
    /// rule the identifier table exists for. Otherwise the room mints under
    /// the swept city and the new anchor is recorded; if the anchor was
    /// claimed concurrently, the linked venue (not the minted one) is where
    /// the facts land.
    ///
    /// `observed_at` stays `None` so the fact's clock is the moment of
    /// ingest — OSM gives no "as of" for a tag.
    pub async fn ingest_element(
        &self,
        city: &SweepCity,
        element: &OsmElement,
    ) -> Result<IngestOutcome> {
        let Some(scheme) = element.scheme() else {
            return Ok(IngestOutcome::Skipped);
        };
        // A nameless element mints nothing — a name somebody typed is the
        // input this layer exists to firm up, and an unnamed node is not it.
        let Some(name) = venue_name(&element.tags) else {
            return Ok(IngestOutcome::Skipped);
        };
        let identifier = element.id.to_string();
        let (latitude, longitude) = element
            .coordinates()
            .map_or((None, None), |(lat, lon)| (Some(lat), Some(lon)));
        let venue_id = match self
            .directory
            .resolve_identifier(scheme, &identifier)
            .await?
        {
            Some(venue_id) => venue_id,
            None => {
                let minted = self
                    .directory
                    .upsert_venue(city.id, name, latitude, longitude)
                    .await?;
                self.directory
                    .link_venue_identifier(minted, scheme, &identifier)
                    .await?
            }
        };
        let source_ref = format!("osm:{}:{}", element.kind, element.id);
        for (attribute, value) in facts_from_tags(&element.tags) {
            PostgresVenueSeedRepository::write_fact(
                &self.database,
                VenueFactWrite {
                    venue_id,
                    attribute,
                    value: &value,
                    provenance: "open_directory",
                    source_ref: &source_ref,
                    observed_at: None,
                    expires_at: None,
                    workspace_id: None,
                    licence: Some("odbl"),
                },
            )
            .await?;
        }
        Ok(IngestOutcome::Written)
    }
}

/// The element's name tag, trimmed. `None` is the skip signal — nothing else
/// about the element gets a chance to write.
fn venue_name(tags: &HashMap<String, String>) -> Option<&str> {
    tags.get("name")
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
}

/// Draw a delay in `[0, max)` from the OS RNG; a broken RNG falls back to no
/// jitter, which is the correct direction — never the other way.
fn jitter(max: Duration) -> Duration {
    let mut bytes = [0_u8; 8];
    if fill_random(&mut bytes).is_err() || max.is_zero() {
        return Duration::ZERO;
    }
    Duration::from_secs(u64::from_le_bytes(bytes) % max.as_secs())
}

/// The tag-to-fact mapping is a whitelist on purpose: an attribute appears
/// here only when an open directory is allowed to claim it.
///
/// What is deliberately absent: `email`, `contact:email`, `phone`,
/// `contact:phone`, `contact:*`, `booking:*` and anything else that smells
/// like a route to the room. An OSM `email` tag is a general inbox somebody
/// left on the map, and `BookingRefusalReason::RouteInferred` exists for
/// exactly this — treating a scraped address as a booking route is the one
/// failure that burns a real relationship. Those tags are never read, so
/// they can never become rows.
pub fn facts_from_tags(tags: &HashMap<String, String>) -> Vec<(&'static str, String)> {
    let tag = |key: &str| {
        tags.get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    };
    let mut facts = Vec::new();

    // The room's own published site — a directory fact, not a route.
    if let Some(website) = ["contact:website", "website", "url"]
        .into_iter()
        .find_map(&tag)
    {
        facts.push(("website", website.to_owned()));
    }

    // The postal address, composed from the parts OSM actually carries.
    let street = [tag("addr:street"), tag("addr:housenumber")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let mut address_parts: Vec<&str> = Vec::new();
    if !street.is_empty() {
        address_parts.push(street.as_str());
    }
    if let Some(city) = tag("addr:city") {
        address_parts.push(city);
    }
    if !address_parts.is_empty() {
        facts.push(("address", address_parts.join(", ")));
    }

    // Capacity only when the tag is an honest integer — "about 400" is not a
    // number the registry will carry.
    if let Some(capacity) = tag("capacity")
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|capacity| *capacity > 0 && *capacity <= 1_000_000)
    {
        facts.push(("capacity", capacity.to_string()));
    }

    // "closed" is written only when the element says so — the absence of a
    // closed record already IS the active claim, and writing both would let
    // a stale "active" outlive a newer "closed".
    let closed = ["closed", "disused"]
        .into_iter()
        .any(|key| tag(key) == Some("yes"));
    if closed {
        facts.push(("status", "closed".to_owned()));
    }

    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_bbox_reaches_past_the_city_pin() {
        let bbox = Bbox::around(51.1079, 17.0385);
        assert!((bbox.south - 50.9579).abs() < 1e-9);
        assert!((bbox.north - 51.2579).abs() < 1e-9);
        assert!((bbox.west - 16.8885).abs() < 1e-9);
        assert!((bbox.east - 17.1885).abs() < 1e-9);
        assert_eq!(bbox.overpass(), "50.9579,16.8885,51.2579,17.1885");
    }

    #[test]
    fn the_bbox_clamps_at_the_edges_of_the_globe() {
        let north_pole = Bbox::around(89.99, -179.99);
        assert_eq!(north_pole.north, 90.0);
        assert_eq!(north_pole.west, -180.0);
        let south_pole = Bbox::around(-89.99, 179.99);
        assert_eq!(south_pole.south, -90.0);
        assert_eq!(south_pole.east, 180.0);
    }

    #[test]
    fn a_canned_overpass_reply_parses() {
        let body = r#"{
            "version": 0.6,
            "generator": "Overpass API",
            "elements": [
                {
                    "type": "node",
                    "id": 302516611,
                    "lat": 51.1080,
                    "lon": 17.0390,
                    "tags": {"amenity": "nightclub", "name": "Stodoła"}
                },
                {
                    "type": "way",
                    "id": 4210007,
                    "center": {"lat": 51.1099, "lon": 17.0311},
                    "tags": {"amenity": "concert_hall", "name": "Sala Koncertowa"}
                }
            ]
        }"#;
        let elements = parse_overpass_body(body.as_bytes()).expect("canned body parses");
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].kind, "node");
        assert_eq!(elements[0].coordinates(), Some((51.1080, 17.0390)));
        // A way has no lat/lon of its own — the pin comes from `center`.
        assert_eq!(elements[1].lat, None);
        assert_eq!(elements[1].coordinates(), Some((51.1099, 17.0311)));
        assert_eq!(elements[1].scheme(), Some("osm_way"));
    }

    #[test]
    fn a_malformed_reply_is_an_error_not_an_empty_city() {
        assert!(parse_overpass_body(b"not json").is_err());
    }

    #[test]
    fn each_tag_lands_on_the_right_attribute() {
        let facts = facts_from_tags(&tags(&[
            ("name", "Alibi"),
            ("contact:website", "https://alibi.example/"),
            ("website", "https://ignored.example/"),
            ("addr:street", "Władysława Reymonta"),
            ("addr:housenumber", "22"),
            ("addr:city", "Wrocław"),
            ("capacity", "350"),
        ]));
        assert_eq!(
            facts,
            vec![
                ("website", "https://alibi.example/".to_owned()),
                ("address", "Władysława Reymonta 22, Wrocław".to_owned()),
                ("capacity", "350".to_owned()),
            ]
        );
    }

    #[test]
    fn website_falls_back_through_the_tag_order() {
        let facts = facts_from_tags(&tags(&[("url", "https://only-url.example/")]));
        assert_eq!(
            facts,
            vec![("website", "https://only-url.example/".to_owned())]
        );
        let facts = facts_from_tags(&tags(&[
            ("website", "https://wins.example/"),
            ("url", "https://loses.example/"),
        ]));
        assert_eq!(facts, vec![("website", "https://wins.example/".to_owned())]);
    }

    #[test]
    fn an_address_composes_whatever_parts_exist() {
        let facts = facts_from_tags(&tags(&[("addr:city", "Poznań")]));
        assert_eq!(facts, vec![("address", "Poznań".to_owned())]);
        let facts = facts_from_tags(&tags(&[("addr:street", "Oławska")]));
        assert_eq!(facts, vec![("address", "Oławska".to_owned())]);
        let facts = facts_from_tags(&tags(&[("addr:housenumber", "9")]));
        assert_eq!(facts, vec![("address", "9".to_owned())]);
    }

    #[test]
    fn capacity_must_be_an_honest_integer() {
        assert!(facts_from_tags(&tags(&[("capacity", "about 400")])).is_empty());
        assert!(facts_from_tags(&tags(&[("capacity", "0")])).is_empty());
        assert!(facts_from_tags(&tags(&[("capacity", "-12")])).is_empty());
        assert_eq!(
            facts_from_tags(&tags(&[("capacity", "250")])),
            vec![("capacity", "250".to_owned())]
        );
    }

    #[test]
    fn only_a_marked_closure_writes_a_status() {
        assert!(facts_from_tags(&tags(&[("opening_hours", "24/7")])).is_empty());
        assert_eq!(
            facts_from_tags(&tags(&[("closed", "yes")])),
            vec![("status", "closed".to_owned())]
        );
        assert_eq!(
            facts_from_tags(&tags(&[("disused", "yes")])),
            vec![("status", "closed".to_owned())]
        );
    }

    #[test]
    fn contact_tags_produce_no_facts_at_all() {
        // The whole point of the whitelist: an OSM email is a general inbox,
        // and RouteInferred is the reason it can never become a booking route.
        let facts = facts_from_tags(&tags(&[
            ("name", "Klub"),
            ("email", "info@klub.example"),
            ("contact:email", "bookings@klub.example"),
            ("contact:phone", "+48 71 000 0000"),
            ("phone", "+48 71 000 0001"),
            ("booking:email", "promo@klub.example"),
        ]));
        assert!(
            facts.is_empty(),
            "contact-shaped tags must write nothing: {facts:?}"
        );
    }

    #[test]
    fn a_nameless_element_is_a_skip_signal() {
        assert_eq!(venue_name(&tags(&[])), None);
        assert_eq!(venue_name(&tags(&[("name", "   ")])), None);
        assert_eq!(venue_name(&tags(&[("name", " Klub ")])), Some("Klub"));
    }

    #[test]
    fn an_element_kind_we_do_not_query_anchors_nothing() {
        let element = OsmElement {
            kind: "relation".to_owned(),
            id: 7,
            lat: Some(1.0),
            lon: Some(2.0),
            center: None,
            tags: HashMap::new(),
        };
        assert_eq!(element.scheme(), None);
    }

    #[test]
    fn jitter_stays_inside_its_bound() {
        for _ in 0..64 {
            assert!(jitter(SWEEP_JITTER_MAX) < SWEEP_JITTER_MAX);
        }
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }
}
