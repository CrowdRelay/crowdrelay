//! Anchors rooms to Ticketmaster and feeds the evidence graph from Discovery
//! (§12-3, Sprint 4V.9).
//!
//! Ticketmaster is the one external source that returns *events with acts
//! attached*. Its coverage is honestly thin for the 200–400-cap rooms Virya
//! cares about and strong for arenas, so it is the second event source, not
//! the first — and it writes onto the map, never under it:
//!
//! - **No venue, no fact.** An event naming a room `place_venues` does not
//!   hold mints nothing — not even the `ticketmaster` identifier. Room
//!   identity is earned by the OSM layer or a played show; a listings feed
//!   does not get to mint rooms.
//! - **Never tenant data.** No `event_acts` (a tenant's own bill), no
//!   contacts, no capacity claims. `event_evidence` facts, `ticketmaster`
//!   anchors, peer acts and peer genres — all global, all idempotent.
//!
//! Provider discipline is structural like the OSM sweep's: one request per
//! city per cycle (size=200 covers a day's worth of a city's listings), a
//! 250 ms floor between cities (Discovery allows 5/s), a 24 h cadence with
//! jitter, and the whole worker sits behind `CROWDRELAY_TICKETMASTER_ENABLED`
//! plus `CROWDRELAY_TICKETMASTER_API_KEY` — both off/absent by default, so a
//! keyless deploy never even starts a request.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

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

use crate::osm_venue_sweep::SweepCity;

/// Discovery allows five requests a second; the floor between two city
/// calls leaves headroom rather than riding the limit.
const PROVIDER_MIN_SPACING: Duration = Duration::from_millis(250);
/// One page of 200 per city per day is the whole sweep — at even a thousand
/// swept cities the daily draw stays far under Discovery's 5000/day cap.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const SWEEP_JITTER_MAX: Duration = Duration::from_secs(60 * 60);
const STARTUP_JITTER_MAX: Duration = Duration::from_secs(5 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const PAGE_SIZE: &str = "200";
/// The Music segment — the sweep is genre-agnostic: every music event is
/// venue evidence, whichever genre the bill is.
const CLASSIFICATION: &str = "music";
const DEFAULT_ENDPOINT: &str = "https://app.ticketmaster.com/discovery/v2/events.json";

/// What one event's ingest did. `Skipped` covers events with no venue, an
/// unnameable venue, or a venue the registry does not hold — counted, never
/// written.
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
    pub events: u32,
    pub written: u32,
    pub skipped: u32,
    pub failed: u32,
}

// ── The Discovery payload — the slices of an event the sweep reads. ──────

#[derive(Clone, Debug, Deserialize)]
pub struct TmEvent {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub dates: Option<TmDates>,
    #[serde(default)]
    pub classifications: Vec<TmClassification>,
    #[serde(default, rename = "_embedded")]
    pub embedded: Option<TmEventEmbedded>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmDates {
    #[serde(default)]
    pub start: Option<TmStart>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmStart {
    #[serde(default, rename = "dateTime")]
    pub date_time: Option<String>,
    /// Ticketmaster's localDate is `YYYY-MM-DD` — a show with no timestamp
    /// still carries the day, which is the granularity the evidence needs.
    #[serde(default, rename = "localDate")]
    pub local_date: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmClassification {
    #[serde(default)]
    pub genre: Option<TmName>,
    #[serde(default, rename = "subGenre")]
    pub sub_genre: Option<TmName>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmName {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmEventEmbedded {
    #[serde(default)]
    pub venues: Vec<TmVenue>,
    #[serde(default)]
    pub attractions: Vec<TmAttraction>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmVenue {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmAttraction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "externalLinks")]
    pub external_links: Option<TmExternalLinks>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmExternalLinks {
    #[serde(default)]
    pub musicbrainz: Vec<TmLink>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TmLink {
    #[serde(default)]
    pub url: Option<String>,
}

/// The Discovery call itself, behind a trait so a test answers from a canned
/// payload rather than the public endpoint. `Err` means "the provider
/// failed", never "the city has no events" — an empty page is `Ok(vec![])`.
#[async_trait]
pub trait TicketmasterProvider: Send + Sync {
    async fn events_in(&self, city: &SweepCity) -> Result<Vec<TmEvent>>;
}

/// The live provider: one GET per city — `size=200` covers a day's listings
/// for any city the sweep selects, so no pagination exists to get wrong.
pub struct TicketmasterClient {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl TicketmasterClient {
    /// An identifying `User-Agent`, like the OSM client sends Overpass.
    pub fn new(endpoint: String, api_key: String, http_timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("CrowdRelay/1.0 ticketmaster-sweep (venue evidence layer)")
            .timeout(http_timeout)
            .connect_timeout(http_timeout.min(Duration::from_secs(10)))
            .build()
            .context("building the Ticketmaster HTTP client")?;
        Ok(Self {
            client,
            endpoint,
            api_key,
        })
    }
}

#[async_trait]
impl TicketmasterProvider for TicketmasterClient {
    async fn events_in(&self, city: &SweepCity) -> Result<Vec<TmEvent>> {
        let response = self
            .client
            .get(&self.endpoint)
            .query(&[
                ("city", city.name.as_str()),
                ("countryCode", city.country_code.as_str()),
                ("classificationName", CLASSIFICATION),
                ("size", PAGE_SIZE),
                ("apikey", self.api_key.as_str()),
            ])
            .send()
            .await
            .context("ticketmaster request failed")?;
        if !response.status().is_success() {
            bail!("ticketmaster returned HTTP {}", response.status());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            bail!("ticketmaster answer exceeds {MAX_RESPONSE_BYTES} bytes");
        }
        let body = response
            .bytes()
            .await
            .context("reading the ticketmaster answer failed")?;
        if body.len() > MAX_RESPONSE_BYTES {
            bail!("ticketmaster answer exceeds {MAX_RESPONSE_BYTES} bytes");
        }
        parse_events_body(&body)
    }
}

/// Reads one Discovery body into events. Separated from the transport so the
/// wire format is testable without a network. A zero-result page carries no
/// `_embedded` at all — absence is an empty list, not a parse failure.
pub fn parse_events_body(body: &[u8]) -> Result<Vec<TmEvent>> {
    #[derive(Deserialize)]
    struct DiscoveryBody {
        #[serde(default, rename = "_embedded")]
        embedded: Option<EventsEmbedded>,
    }
    #[derive(Deserialize)]
    struct EventsEmbedded {
        #[serde(default)]
        events: Vec<TmEvent>,
    }
    let parsed = serde_json::from_slice::<DiscoveryBody>(body)
        .context("ticketmaster answer was not the expected JSON")?;
    Ok(parsed
        .embedded
        .map(|embedded| embedded.events)
        .unwrap_or_default())
}

pub struct TicketmasterSweepWorker {
    database: PgPool,
    provider: Arc<dyn TicketmasterProvider>,
    directory: PostgresVenueDirectoryRepository,
    sweep_interval: Duration,
    operation_timeout: Duration,
}

impl TicketmasterSweepWorker {
    #[must_use]
    pub fn new(
        database: PgPool,
        provider: Arc<dyn TicketmasterProvider>,
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

    /// The flag-and-key decision main.rs would otherwise inline: enabled
    /// without a key — or a client that cannot build — stays inert with one
    /// log line rather than failing the worker boot. `None` means "do not
    /// spawn"; the caller never branches on configuration itself.
    pub fn maybe_standard(
        database: PgPool,
        enabled: bool,
        api_key: Option<String>,
        operation_timeout: Duration,
    ) -> Option<Self> {
        if !enabled {
            tracing::info!(
                "Ticketmaster sweep is disabled; set CROWDRELAY_TICKETMASTER_ENABLED=true with an \
                 API key to write event_evidence facts and peer acts from Discovery"
            );
            return None;
        }
        let Some(api_key) = api_key else {
            tracing::warn!(
                "Ticketmaster sweep is enabled but CROWDRELAY_TICKETMASTER_API_KEY is unset; \
                 the sweep stays inert"
            );
            return None;
        };
        match Self::standard(database, api_key, operation_timeout) {
            Ok(worker) => Some(worker),
            Err(error) => {
                tracing::warn!(error = %error, "Ticketmaster sweep disabled: HTTP client build failed");
                None
            }
        }
    }

    /// The production wiring: the public Discovery endpoint, or the mirror
    /// `CROWDRELAY_TICKETMASTER_EVENTS_URL` names.
    pub fn standard(
        database: PgPool,
        api_key: String,
        operation_timeout: Duration,
    ) -> Result<Self> {
        let endpoint = std::env::var("CROWDRELAY_TICKETMASTER_EVENTS_URL")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned());
        let provider =
            TicketmasterClient::new(endpoint, api_key, operation_timeout.max(HTTP_TIMEOUT))?;
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
            // Drawn rather than a plain interval so a fleet of deployments
            // does not hit Discovery in lockstep — same shape as the OSM
            // sweep, and shutdown still answers immediately.
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
                tracing::warn!(%error, "Ticketmaster sweep cycle failed");
            }
        }
    }

    /// One cycle: the swept cities (identical selection to the OSM sweep —
    /// "cities worth sweeping" is one judgement, not two), then one
    /// Discovery call per city at the provider's spacing. A failed city is
    /// logged and left for the next cycle — it never cancels the rest.
    pub async fn sweep_once(&self) -> Result<SweepSummary> {
        let cities = tokio::time::timeout(self.operation_timeout, self.sweep_cities())
            .await
            .context("selecting cities for the Ticketmaster sweep")??;
        let mut summary = SweepSummary::default();
        for (index, city) in cities.iter().enumerate() {
            if index > 0 {
                sleep(PROVIDER_MIN_SPACING).await;
            }
            match self.provider.events_in(city).await {
                Ok(events) => {
                    summary.cities += 1;
                    for event in &events {
                        summary.events += 1;
                        // The database budget bounds the write, same as every
                        // other worker: a stalled pool must not pin the sweep.
                        match tokio::time::timeout(
                            self.operation_timeout,
                            self.ingest_event(city, event),
                        )
                        .await
                        {
                            Ok(Ok(IngestOutcome::Written)) => summary.written += 1,
                            Ok(Ok(IngestOutcome::Skipped)) => summary.skipped += 1,
                            Ok(Err(error)) => {
                                summary.failed += 1;
                                tracing::warn!(
                                    city = %city.slug,
                                    event_id = %event.id,
                                    %error,
                                    "Ticketmaster event ingest failed"
                                );
                            }
                            Err(_) => {
                                summary.failed += 1;
                                tracing::warn!(
                                    city = %city.slug,
                                    event_id = %event.id,
                                    "Ticketmaster event ingest timed out"
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    summary.city_failures += 1;
                    tracing::warn!(city = %city.slug, %error, "ticketmaster query failed for city");
                }
            }
        }
        tracing::info!(
            cities = summary.cities,
            city_failures = summary.city_failures,
            events = summary.events,
            events_written = summary.written,
            events_skipped = summary.skipped,
            events_failed = summary.failed,
            "Ticketmaster sweep complete"
        );
        Ok(summary)
    }

    /// The swept city set — the same query the OSM sweep runs, deliberately:
    /// anywhere any tenant has a reachable fan or a show is worth evidence.
    /// The coordinate predicate stays even though Discovery queries by name,
    /// because "the swept cities" is one set across the external layer.
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
        .context("selecting cities for the Ticketmaster sweep")
    }

    /// One event: resolve the room, then anchor, fact, peers, genres.
    ///
    /// The anchor answers before the name does — `(ticketmaster, id)` is the
    /// strongest identity a source can carry (§12-4). A name+city match is
    /// the fallback, and matching wins the anchor for next time. A room the
    /// registry does not hold ends the event's writes entirely: the sweep is
    /// evidence about rooms that exist, never a minting path.
    pub async fn ingest_event(&self, city: &SweepCity, event: &TmEvent) -> Result<IngestOutcome> {
        let Some(venue) = event
            .embedded
            .as_ref()
            .and_then(|embedded| embedded.venues.first())
        else {
            return Ok(IngestOutcome::Skipped);
        };
        let venue_name = venue
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let venue_id = match venue
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(tm_venue_id) => {
                match self
                    .directory
                    .resolve_identifier("ticketmaster", tm_venue_id)
                    .await?
                {
                    Some(venue_id) => venue_id,
                    None => {
                        let Some(venue_name) = venue_name else {
                            return Ok(IngestOutcome::Skipped);
                        };
                        match self.resolve_by_name(city.id, venue_name).await? {
                            // The linked id, not the matched one: the anchor
                            // may already belong to this room's earlier name.
                            Some(matched) => {
                                self.directory
                                    .link_venue_identifier(matched, "ticketmaster", tm_venue_id)
                                    .await?
                            }
                            // No venue, no fact — the sweep mints nothing.
                            None => return Ok(IngestOutcome::Skipped),
                        }
                    }
                }
            }
            None => {
                let Some(venue_name) = venue_name else {
                    return Ok(IngestOutcome::Skipped);
                };
                match self.resolve_by_name(city.id, venue_name).await? {
                    Some(venue_id) => venue_id,
                    None => return Ok(IngestOutcome::Skipped),
                }
            }
        };

        // The event_evidence fact: one row per (venue, event) — a show
        // happened, named and dated. `source_ref` is the event id, so a
        // re-sweep refreshes the claim rather than stacking a twin.
        let source_ref = format!("ticketmaster:{}", event.id);
        let starts_at = event_start(event);
        let value = fact_value(event);
        PostgresVenueSeedRepository::write_fact(
            &self.database,
            VenueFactWrite {
                venue_id,
                attribute: "show",
                value: &value,
                provenance: "event_evidence",
                source_ref: &source_ref,
                observed_at: starts_at,
                expires_at: None,
                workspace_id: None,
                licence: None,
            },
        )
        .await?;

        // The bill's acts are global peers — mint-or-link on the normalized
        // name, anchor the MBID when the attraction carries one, then tag the
        // event's genres onto each act. `event_acts` is never touched: those
        // rows are a tenant's own bill, and a Ticketmaster show is not one.
        let genre_tags = genre_tags(&event.classifications);
        if let Some(embedded) = &event.embedded {
            for attraction in &embedded.attractions {
                let Some(name) = attraction
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                else {
                    continue;
                };
                let mbid = attraction_mbid(attraction);
                let Some(peer_id) = self.peer_act(name, mbid).await? else {
                    continue;
                };
                for tag in &genre_tags {
                    sqlx::query(
                        r#"
                        INSERT INTO place_peer_act_genres
                            (peer_act_id, genre_tag, provenance, source_ref)
                        VALUES ($1, $2, 'event_evidence', $3)
                        ON CONFLICT DO NOTHING
                        "#,
                    )
                    .bind(peer_id)
                    .bind(tag)
                    .bind(&source_ref)
                    .execute(&self.database)
                    .await?;
                }
            }
        }
        Ok(IngestOutcome::Written)
    }

    /// The name+city match: `place_venue_key` normalization — the same rule
    /// every other writer resolves rooms by. No fuzzy anything; a miss is a
    /// skip, not a guess.
    async fn resolve_by_name(&self, city_id: Uuid, name: &str) -> Result<Option<Uuid>> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id
            FROM place_venues
            WHERE city_id = $1 AND name_key = place_venue_key($2)
            "#,
        )
        .bind(city_id)
        .bind(name)
        .fetch_optional(&self.database)
        .await
        .context("resolving a Ticketmaster venue by name")
    }

    /// Mint-or-link one billed act (§12-5 entity 1). Identity is the
    /// `place_venue_key`-normalized name; an MBID is the stronger anchor when
    /// Ticketmaster carries one — claimed only when no other act holds it,
    /// and never overwriting a name a tenant already knows.
    async fn peer_act(&self, name: &str, mbid: Option<Uuid>) -> Result<Option<Uuid>> {
        // The CHECK bounds display_name/name_key at 500; the cap lands before
        // the key is derived so the two stay consistent.
        let display: String = name.chars().take(500).collect();
        let name_key = sqlx::query_scalar::<_, Option<String>>("SELECT place_venue_key($1)")
            .bind(&display)
            .fetch_one(&self.database)
            .await
            .context("normalizing a peer act name")?;
        let Some(name_key) = name_key else {
            // A name with no normalizable content is no act — "!!!" is not a
            // band, it is punctuation.
            return Ok(None);
        };
        if let Some(existing) =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM place_peer_acts WHERE name_key = $1")
                .bind(&name_key)
                .fetch_optional(&self.database)
                .await
                .context("resolving a peer act by name")?
        {
            // Anchor the MBID if the act lacks one and no other act owns it —
            // the unique index is the arbiter, this WHERE is the politeness.
            if let Some(mbid) = mbid {
                sqlx::query(
                    r#"
                    UPDATE place_peer_acts SET mbid = $2
                    WHERE id = $1 AND mbid IS NULL
                      AND NOT EXISTS (SELECT 1 FROM place_peer_acts WHERE mbid = $2)
                    "#,
                )
                .bind(existing)
                .bind(mbid)
                .execute(&self.database)
                .await
                .context("anchoring a peer act MBID")?;
            }
            return Ok(Some(existing));
        }
        let minted = sqlx::query_scalar::<_, Option<Uuid>>(
            r#"
            INSERT INTO place_peer_acts (name_key, display_name, mbid)
            VALUES ($1, $2, $3)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(&name_key)
        .bind(&display)
        .bind(mbid)
        .fetch_optional(&self.database)
        .await
        .context("minting a peer act")?;
        match minted.flatten() {
            Some(id) => Ok(Some(id)),
            None => {
                // The insert's arbiter is bare `ON CONFLICT DO NOTHING` — it
                // swallows a name_key race *and* an MBID another act already
                // owns. Read the name's winner; if the miss was the MBID,
                // mint the act without the anchor rather than lose the act.
                if let Some(id) = sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM place_peer_acts WHERE name_key = $1",
                )
                .bind(&name_key)
                .fetch_optional(&self.database)
                .await
                .context("re-reading a peer act")?
                {
                    return Ok(Some(id));
                }
                let id = sqlx::query_scalar::<_, Option<Uuid>>(
                    r#"
                    INSERT INTO place_peer_acts (name_key, display_name)
                    VALUES ($1, $2)
                    ON CONFLICT (name_key) DO NOTHING
                    RETURNING id
                    "#,
                )
                .bind(&name_key)
                .bind(&display)
                .fetch_optional(&self.database)
                .await
                .context("minting a peer act without its MBID")?;
                match id.flatten() {
                    Some(id) => Ok(Some(id)),
                    None => sqlx::query_scalar::<_, Uuid>(
                        "SELECT id FROM place_peer_acts WHERE name_key = $1",
                    )
                    .bind(&name_key)
                    .fetch_optional(&self.database)
                    .await
                    .context("final peer act re-read"),
                }
            }
        }
    }
}

/// The show's moment: `dates.start.dateTime` when the event carries one,
/// else the local date at midnight UTC. `None` means the fact's clock is the
/// ingest time — `write_fact` supplies `now()`.
fn event_start(event: &TmEvent) -> Option<time::OffsetDateTime> {
    let start = event.dates.as_ref()?.start.as_ref()?;
    if let Some(date_time) = &start.date_time
        && let Ok(parsed) =
            time::OffsetDateTime::parse(date_time, &time::format_description::well_known::Rfc3339)
    {
        return Some(parsed);
    }
    start.local_date.as_ref().and_then(|local_date| {
        time::Date::parse(
            local_date,
            &time::macros::format_description!("[year]-[month]-[day]"),
        )
        .ok()
        .map(|date| date.midnight().assume_utc())
    })
}

/// The fact's value: the show's name plus its day — "Metallica · 2026-11-14".
/// A nameless event still names itself by id; an empty value would be a fact
/// that says nothing.
fn fact_value(event: &TmEvent) -> String {
    let name = event
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(|| format!("event {}", event.id), str::to_owned);
    let date = event
        .dates
        .as_ref()
        .and_then(|dates| dates.start.as_ref())
        .and_then(|start| start.local_date.clone())
        .or_else(|| event_start(event).map(|starts_at| starts_at.date().to_string()));
    match date {
        Some(date) => format!("{name} · {date}"),
        None => name,
    }
}

/// The act's MusicBrainz anchor, when the attraction's external links carry
/// one — the UUID is the URL's last segment.
fn attraction_mbid(attraction: &TmAttraction) -> Option<Uuid> {
    attraction
        .external_links
        .as_ref()?
        .musicbrainz
        .iter()
        .filter_map(|link| link.url.as_deref())
        .find_map(|url| {
            url.trim_end_matches('/')
                .rsplit('/')
                .next()
                .and_then(|segment| Uuid::parse_str(segment).ok())
        })
}

/// The event's genre claims — genre and subGenre names across every
/// classification, deduped and with Ticketmaster's "Undefined" placeholder
/// dropped: a tag that says nothing is not a tag.
fn genre_tags(classifications: &[TmClassification]) -> BTreeSet<String> {
    let mut tags = BTreeSet::new();
    for classification in classifications {
        for name in [
            classification
                .genre
                .as_ref()
                .and_then(|genre| genre.name.as_deref()),
            classification
                .sub_genre
                .as_ref()
                .and_then(|sub| sub.name.as_deref()),
        ]
        .into_iter()
        .flatten()
        {
            let tag: String = name.trim().chars().take(100).collect();
            if !tag.is_empty() && !tag.eq_ignore_ascii_case("undefined") {
                tags.insert(tag);
            }
        }
    }
    tags
}

/// Draw a delay in `[0, max)` from the OS RNG; a broken RNG falls back to no
/// jitter, which is the correct direction — never the other way.
fn jitter(max: Duration) -> Duration {
    let mut bytes = [0u8; 8];
    if fill_random(&mut bytes).is_err() {
        return Duration::ZERO;
    }
    let max_ms = u64::try_from(max.as_millis()).unwrap_or(u64::MAX);
    if max_ms == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(u64::from_le_bytes(bytes) % max_ms)
}
