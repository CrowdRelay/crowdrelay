//! Idempotent, transactionally audited bootstrap of operator-owned workspace data.
//!
//! This module deliberately accepts secret references only. It never resolves,
//! logs, or returns secret material and it redacts configured URLs from `Debug`.

use std::{collections::HashSet, fmt, time::Duration};

use crowdrelay_domain::{
    CitySlug, CountryCode, DestinationUrl, SmartLinkSlug, WorkspaceId, WorkspaceSlug,
};
use crowdrelay_infra::config::DatabaseConfig;
use serde::Deserialize;
use serde_json::json;
use sha2::Digest;
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::time::timeout;
use uuid::Uuid;

const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_NAME_BYTES: usize = 200;
const MAX_CITY_NAME_BYTES: usize = 160;
const MAX_REGION_BYTES: usize = 200;
const MAX_CITIES: usize = 1_000;
const MAX_CAMPAIGNS: usize = 100;
const MAX_SMART_LINKS_PER_CAMPAIGN: usize = 1_000;
const MAX_SMART_LINKS: usize = 5_000;
const MAX_WEBHOOK_ENDPOINTS: usize = 100;
const MAX_REWARD_RULES: usize = 100;
const MAX_EVENTS: usize = 1_000;
const MAX_ADMISSION_POOLS: usize = 1_000;
const MAX_EVENT_SOURCES: usize = 32;
const MAX_REWARD_DRAWS: usize = 1_000;
const MAX_EVENT_TITLE_BYTES: usize = 300;
const MAX_EVENT_DESCRIPTION_BYTES: usize = 10_000;
const MAX_EVENT_TEXT_BYTES: usize = 500;
const MAX_TIMEZONE_BYTES: usize = 128;
const MIN_WEBHOOK_TIMEOUT_MS: u32 = 100;
const MAX_WEBHOOK_TIMEOUT_MS: u32 = 60_000;
const MIN_WEBHOOK_ATTEMPTS: u16 = 1;
const MAX_WEBHOOK_ATTEMPTS: u16 = 100;
const MAX_SECRET_REFERENCE_BYTES: usize = 128;
const BOOTSTRAP_LOCK_PREFIX: &str = "crowdrelay:workspace-bootstrap:";
const AUDIT_ACTION: &str = "workspace.bootstrap.applied";

/// A fully parsed and validated bootstrap document.
///
/// Fields stay private so unvalidated instances cannot be constructed by the
/// composition root. Use [`BootstrapSpec::parse`] for every document.
#[derive(Clone)]
pub struct BootstrapSpec {
    workspace_name: String,
    cities: Vec<CitySpec>,
    campaigns: Vec<CampaignSpec>,
    webhook_endpoints: Vec<WebhookEndpointSpec>,
    event_sources: Vec<EventSourceSpec>,
    reward_rules: Vec<RewardRuleSpec>,
    events: Vec<EventSpec>,
    admission_pools: Vec<AdmissionPoolSpec>,
    reward_draws: Vec<RewardDrawSpec>,
}

impl BootstrapSpec {
    /// Parses a bounded JSON document and applies environment-specific policy.
    ///
    /// Production documents require HTTPS for both redirect destinations and
    /// webhook endpoints. Unknown fields are rejected at every object level.
    pub fn parse(json: &str, production: bool) -> Result<Self, BootstrapSpecError> {
        if json.len() > MAX_DOCUMENT_BYTES {
            return Err(BootstrapSpecError::DocumentTooLarge {
                max_bytes: MAX_DOCUMENT_BYTES,
            });
        }

        let raw: RawBootstrapSpec =
            serde_json::from_str(json).map_err(|_| BootstrapSpecError::InvalidJson)?;
        ensure_count("cities", raw.cities.len(), MAX_CITIES)?;
        ensure_count("campaigns", raw.campaigns.len(), MAX_CAMPAIGNS)?;
        ensure_count(
            "webhook_endpoints",
            raw.webhook_endpoints.len(),
            MAX_WEBHOOK_ENDPOINTS,
        )?;
        ensure_count("event_sources", raw.event_sources.len(), MAX_EVENT_SOURCES)?;
        ensure_count("reward_rules", raw.reward_rules.len(), MAX_REWARD_RULES)?;
        ensure_count("events", raw.events.len(), MAX_EVENTS)?;
        ensure_count(
            "admission_pools",
            raw.admission_pools.len(),
            MAX_ADMISSION_POOLS,
        )?;
        ensure_count("reward_draws", raw.reward_draws.len(), MAX_REWARD_DRAWS)?;

        let workspace_name = validate_name(raw.workspace_name, "workspace_name")?;
        let mut city_slugs = HashSet::with_capacity(raw.cities.len());
        let mut cities = Vec::with_capacity(raw.cities.len());
        for raw_city in raw.cities {
            let slug =
                CitySlug::parse(&raw_city.slug).map_err(|_| invalid_field("cities[].slug"))?;
            // Fan signup currently resolves a city by slug, so accepting the
            // same slug twice would make acquisition ambiguous even when the
            // country codes differ.
            if !city_slugs.insert(slug.clone()) {
                return Err(duplicate("city slug"));
            }

            let country = CountryCode::parse(&raw_city.country)
                .map_err(|_| invalid_field("cities[].country"))?;
            let (latitude, longitude) = validate_coordinates(raw_city.lat, raw_city.lng)?;
            cities.push(CitySpec {
                slug,
                name: validate_text(raw_city.name, "cities[].name", MAX_CITY_NAME_BYTES)?,
                country,
                region: validate_optional_text(
                    raw_city.region,
                    "cities[].region",
                    MAX_REGION_BYTES,
                )?,
                latitude,
                longitude,
            });
        }

        let mut campaign_names = HashSet::with_capacity(raw.campaigns.len());
        let mut smart_link_slugs = HashSet::new();
        let mut smart_link_count = 0_usize;
        let mut campaigns = Vec::with_capacity(raw.campaigns.len());
        for raw_campaign in raw.campaigns {
            ensure_count(
                "campaigns[].smart_links",
                raw_campaign.smart_links.len(),
                MAX_SMART_LINKS_PER_CAMPAIGN,
            )?;
            smart_link_count = smart_link_count
                .checked_add(raw_campaign.smart_links.len())
                .ok_or(BootstrapSpecError::TooMany {
                    collection: "smart_links",
                    max: MAX_SMART_LINKS,
                })?;
            ensure_count("smart_links", smart_link_count, MAX_SMART_LINKS)?;

            let name = validate_name(raw_campaign.name, "campaigns[].name")?;
            if !campaign_names.insert(name.clone()) {
                return Err(duplicate("campaign name"));
            }

            let mut smart_links = Vec::with_capacity(raw_campaign.smart_links.len());
            for raw_link in raw_campaign.smart_links {
                let slug = SmartLinkSlug::parse(&raw_link.slug)
                    .map_err(|_| invalid_field("campaigns[].smart_links[].slug"))?;
                if !smart_link_slugs.insert(slug.clone()) {
                    return Err(duplicate("smart-link slug"));
                }

                let destination_url = DestinationUrl::parse(&raw_link.destination_url)
                    .map_err(|_| invalid_field("campaigns[].smart_links[].destination_url"))?;
                ensure_environment_url(
                    &destination_url,
                    production,
                    "campaigns[].smart_links[].destination_url",
                )?;
                smart_links.push(SmartLinkSpec {
                    slug,
                    destination_url,
                    active: raw_link.active,
                });
            }

            campaigns.push(CampaignSpec {
                name,
                active: raw_campaign.active,
                smart_links,
            });
        }

        let mut endpoint_names = HashSet::with_capacity(raw.webhook_endpoints.len());
        let mut webhook_endpoints = Vec::with_capacity(raw.webhook_endpoints.len());
        for raw_endpoint in raw.webhook_endpoints {
            let name = validate_name(raw_endpoint.name, "webhook_endpoints[].name")?;
            if !endpoint_names.insert(name.clone()) {
                return Err(duplicate("webhook endpoint name"));
            }

            let url = DestinationUrl::parse(&raw_endpoint.url)
                .map_err(|_| invalid_field("webhook_endpoints[].url"))?;
            ensure_environment_url(&url, production, "webhook_endpoints[].url")?;
            if url.as_str().contains('#') {
                return Err(invalid_field("webhook_endpoints[].url"));
            }
            if !valid_secret_reference(&raw_endpoint.signing_secret_ref) {
                return Err(invalid_field("webhook_endpoints[].signing_secret_ref"));
            }
            if !(MIN_WEBHOOK_TIMEOUT_MS..=MAX_WEBHOOK_TIMEOUT_MS).contains(&raw_endpoint.timeout_ms)
            {
                return Err(invalid_field("webhook_endpoints[].timeout_ms"));
            }
            if !(MIN_WEBHOOK_ATTEMPTS..=MAX_WEBHOOK_ATTEMPTS).contains(&raw_endpoint.max_attempts) {
                return Err(invalid_field("webhook_endpoints[].max_attempts"));
            }

            webhook_endpoints.push(WebhookEndpointSpec {
                name,
                url,
                signing_secret_ref: raw_endpoint.signing_secret_ref,
                timeout_ms: raw_endpoint.timeout_ms,
                max_attempts: raw_endpoint.max_attempts,
                active: raw_endpoint.active,
            });
        }

        let mut event_source_keys = HashSet::with_capacity(raw.event_sources.len());
        let mut event_sources = Vec::with_capacity(raw.event_sources.len());
        for raw_source in raw.event_sources {
            if raw_source.provider != "bandsintown" {
                return Err(invalid_field("event_sources[].provider"));
            }
            let artist_name = validate_text(
                raw_source.artist_name,
                "event_sources[].artist_name",
                MAX_NAME_BYTES,
            )?;
            if !event_source_keys.insert((raw_source.provider.clone(), artist_name.clone())) {
                return Err(duplicate("event source"));
            }
            let app_id =
                validate_text(raw_source.app_id, "event_sources[].app_id", MAX_NAME_BYTES)?;
            let default_country_code = CountryCode::parse(&raw_source.default_country_code)
                .map_err(|_| invalid_field("event_sources[].default_country_code"))?;
            let timezone = validate_text(
                raw_source.timezone,
                "event_sources[].timezone",
                MAX_TIMEZONE_BYTES,
            )?;
            if !(300..=86_400).contains(&raw_source.sync_interval_seconds) {
                return Err(invalid_field("event_sources[].sync_interval_seconds"));
            }
            event_sources.push(EventSourceSpec {
                provider: raw_source.provider,
                artist_name,
                app_id,
                default_country_code,
                timezone,
                sync_interval_seconds: raw_source.sync_interval_seconds,
                active: raw_source.active,
            });
        }

        let mut reward_rule_names = HashSet::with_capacity(raw.reward_rules.len());
        let mut reward_rules = Vec::with_capacity(raw.reward_rules.len());
        for raw_rule in raw.reward_rules {
            let name = validate_name(raw_rule.name, "reward_rules[].name")?;
            if !reward_rule_names.insert(name.clone()) {
                return Err(duplicate("reward rule name"));
            }
            if raw_rule
                .threshold
                .is_some_and(|threshold| !(1..=10_000).contains(&threshold))
            {
                return Err(invalid_field("reward_rules[].threshold"));
            }
            if !(1..=365).contains(&raw_rule.expires_days) {
                return Err(invalid_field("reward_rules[].expires_days"));
            }

            let config = match raw_rule.kind.as_str() {
                "merch_discount" => {
                    if raw_rule.threshold.is_none()
                        || raw_rule.item_name.is_some()
                        || raw_rule.sku.is_some()
                    {
                        return Err(invalid_field("reward_rules[].kind"));
                    }
                    let discount_percent = raw_rule
                        .discount_percent
                        .ok_or(invalid_field("reward_rules[].discount_percent"))?;
                    if !(discount_percent.is_finite()
                        && 0.0 < discount_percent
                        && discount_percent <= 100.0)
                    {
                        return Err(invalid_field("reward_rules[].discount_percent"));
                    }
                    let code_prefix = raw_rule
                        .code_prefix
                        .ok_or(invalid_field("reward_rules[].code_prefix"))?;
                    if !(2..=16).contains(&code_prefix.len())
                        || !code_prefix.bytes().all(|byte| byte.is_ascii_uppercase())
                    {
                        return Err(invalid_field("reward_rules[].code_prefix"));
                    }
                    RewardRuleConfig::MerchDiscount {
                        discount_percent,
                        code_prefix,
                    }
                }
                "physical_item" => {
                    if raw_rule.discount_percent.is_some() || raw_rule.code_prefix.is_some() {
                        return Err(invalid_field("reward_rules[].kind"));
                    }
                    let item_name = validate_text(
                        raw_rule
                            .item_name
                            .ok_or(invalid_field("reward_rules[].item_name"))?,
                        "reward_rules[].item_name",
                        MAX_NAME_BYTES,
                    )?;
                    let sku = raw_rule.sku.ok_or(invalid_field("reward_rules[].sku"))?;
                    if !(1..=64).contains(&sku.len())
                        || !sku
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                    {
                        return Err(invalid_field("reward_rules[].sku"));
                    }
                    RewardRuleConfig::PhysicalItem { item_name, sku }
                }
                _ => return Err(invalid_field("reward_rules[].kind")),
            };

            reward_rules.push(RewardRuleSpec {
                name,
                threshold: raw_rule.threshold,
                expires_days: raw_rule.expires_days,
                active: raw_rule.active,
                config,
            });
        }

        let mut event_slugs = HashSet::with_capacity(raw.events.len());
        let mut events = Vec::with_capacity(raw.events.len());
        for raw_event in raw.events {
            let slug = crowdrelay_domain::EventSlug::parse(&raw_event.slug)
                .map_err(|_| invalid_field("events[].slug"))?;
            if !event_slugs.insert(slug.clone()) {
                return Err(duplicate("event slug"));
            }
            let status = match raw_event.status.as_str() {
                "draft" | "published" | "cancelled" | "completed" => raw_event.status,
                _ => return Err(invalid_field("events[].status")),
            };
            let starts_at = parse_rfc3339(&raw_event.starts_at, "events[].starts_at")?;
            let doors_at =
                parse_optional_rfc3339(raw_event.doors_at.as_deref(), "events[].doors_at")?;
            let ends_at = parse_optional_rfc3339(raw_event.ends_at.as_deref(), "events[].ends_at")?;
            if doors_at.is_some_and(|value| value > starts_at)
                || ends_at.is_some_and(|value| value < starts_at)
            {
                return Err(invalid_field("events[].schedule"));
            }
            let city_slug = raw_event
                .city_slug
                .map(|value| {
                    crowdrelay_domain::CitySlug::parse(value)
                        .map_err(|_| invalid_field("events[].city_slug"))
                })
                .transpose()?;
            let ticket_url =
                parse_optional_url(raw_event.ticket_url, production, "events[].ticket_url")?;
            let listen_url =
                parse_optional_url(raw_event.listen_url, production, "events[].listen_url")?;
            let image_url =
                parse_optional_url(raw_event.image_url, production, "events[].image_url")?;
            let trailer_url =
                parse_optional_url(raw_event.trailer_url, production, "events[].trailer_url")?;
            let external_event_url = parse_optional_url(
                raw_event.external_event_url,
                production,
                "events[].external_event_url",
            )?;
            events.push(EventSpec {
                slug,
                city_slug,
                title: validate_text(raw_event.title, "events[].title", MAX_EVENT_TITLE_BYTES)?,
                description: validate_optional_text(
                    raw_event.description,
                    "events[].description",
                    MAX_EVENT_DESCRIPTION_BYTES,
                )?,
                venue: validate_optional_text(
                    raw_event.venue,
                    "events[].venue",
                    MAX_EVENT_TEXT_BYTES,
                )?,
                venue_address: validate_optional_text(
                    raw_event.venue_address,
                    "events[].venue_address",
                    MAX_EVENT_TEXT_BYTES,
                )?,
                timezone: validate_text(
                    raw_event.timezone,
                    "events[].timezone",
                    MAX_TIMEZONE_BYTES,
                )?,
                starts_at,
                doors_at,
                ends_at,
                ticket_url,
                listen_url,
                image_url,
                trailer_url,
                external_event_url,
                status,
            });
        }

        let mut pool_keys = HashSet::with_capacity(raw.admission_pools.len());
        let mut admission_pools = Vec::with_capacity(raw.admission_pools.len());
        for raw_pool in raw.admission_pools {
            let event_slug = crowdrelay_domain::EventSlug::parse(&raw_pool.event_slug)
                .map_err(|_| invalid_field("admission_pools[].event_slug"))?;
            let slug = crowdrelay_domain::EventSlug::parse(&raw_pool.slug)
                .map_err(|_| invalid_field("admission_pools[].slug"))?;
            if !pool_keys.insert((event_slug.clone(), slug.clone())) {
                return Err(duplicate("admission pool"));
            }
            if !(1..=10_000).contains(&raw_pool.capacity) {
                return Err(invalid_field("admission_pools[].capacity"));
            }
            admission_pools.push(AdmissionPoolSpec {
                event_slug,
                slug,
                name: validate_name(raw_pool.name, "admission_pools[].name")?,
                capacity: raw_pool.capacity,
                active: raw_pool.active,
            });
        }

        let mut draw_slugs = HashSet::with_capacity(raw.reward_draws.len());
        let mut reward_draws = Vec::with_capacity(raw.reward_draws.len());
        for raw_draw in raw.reward_draws {
            let slug = crowdrelay_domain::EventSlug::parse(&raw_draw.slug)
                .map_err(|_| invalid_field("reward_draws[].slug"))?;
            if !draw_slugs.insert(slug.clone()) {
                return Err(duplicate("reward draw slug"));
            }
            let name = validate_name(raw_draw.name, "reward_draws[].name")?;
            let event_slug = raw_draw
                .event_slug
                .map(|value| {
                    crowdrelay_domain::EventSlug::parse(value)
                        .map_err(|_| invalid_field("reward_draws[].event_slug"))
                })
                .transpose()?;
            let admission_pool_slug = raw_draw
                .admission_pool_slug
                .map(|value| {
                    crowdrelay_domain::EventSlug::parse(value)
                        .map_err(|_| invalid_field("reward_draws[].admission_pool_slug"))
                })
                .transpose()?;
            let opens_at = parse_rfc3339(&raw_draw.opens_at, "reward_draws[].opens_at")?;
            let closes_at = parse_rfc3339(&raw_draw.closes_at, "reward_draws[].closes_at")?;
            let draw_at = parse_rfc3339(&raw_draw.draw_at, "reward_draws[].draw_at")?;
            if !(opens_at < closes_at && closes_at <= draw_at) {
                return Err(invalid_field("reward_draws[].schedule"));
            }
            if !(1..=10_000).contains(&raw_draw.winner_count)
                || !(1..=100_000).contains(&raw_draw.base_entries)
                || raw_draw.entries_per_referral > 100_000
                || raw_draw.entries_per_checkin > 100_000
                || !(1..=1_000_000).contains(&raw_draw.max_entries)
                || raw_draw.max_entries < raw_draw.base_entries
                || !(1..=8_760).contains(&raw_draw.claim_expires_hours)
            {
                return Err(invalid_field("reward_draws[].weights"));
            }
            if !matches!(
                raw_draw.status.as_str(),
                "draft" | "scheduled" | "cancelled"
            ) {
                return Err(invalid_field("reward_draws[].status"));
            }
            if !matches!(
                raw_draw.eligibility_kind.as_str(),
                "all_active" | "event_interest"
            ) {
                return Err(invalid_field("reward_draws[].eligibility_kind"));
            }
            if raw_draw.eligibility_kind == "event_interest" && event_slug.is_none() {
                return Err(invalid_field("reward_draws[].event_slug"));
            }
            if event_slug
                .as_ref()
                .is_some_and(|value| !event_slugs.contains(value))
            {
                return Err(invalid_field("reward_draws[].event_slug"));
            }
            match raw_draw.prize_kind.as_str() {
                "admission_pass" => {
                    let Some(event_slug_value) = event_slug.as_ref() else {
                        return Err(invalid_field("reward_draws[].event_slug"));
                    };
                    let Some(pool_slug_value) = admission_pool_slug.as_ref() else {
                        return Err(invalid_field("reward_draws[].admission_pool_slug"));
                    };
                    if raw_draw.reward_rule_name.is_some()
                        || !pool_keys.contains(&(event_slug_value.clone(), pool_slug_value.clone()))
                    {
                        return Err(invalid_field("reward_draws[].admission_pool_slug"));
                    }
                }
                "physical_item" => {
                    if admission_pool_slug.is_some()
                        || raw_draw
                            .reward_rule_name
                            .as_ref()
                            .is_none_or(|value| !reward_rule_names.contains(value))
                    {
                        return Err(invalid_field("reward_draws[].reward_rule_name"));
                    }
                }
                _ => return Err(invalid_field("reward_draws[].prize_kind")),
            }
            reward_draws.push(RewardDrawSpec {
                slug,
                name,
                prize_kind: raw_draw.prize_kind,
                eligibility_kind: raw_draw.eligibility_kind,
                event_slug,
                admission_pool_slug,
                reward_rule_name: raw_draw.reward_rule_name,
                winner_count: raw_draw.winner_count,
                base_entries: raw_draw.base_entries,
                entries_per_referral: raw_draw.entries_per_referral,
                entries_per_checkin: raw_draw.entries_per_checkin,
                max_entries: raw_draw.max_entries,
                claim_expires_hours: raw_draw.claim_expires_hours,
                opens_at,
                closes_at,
                draw_at,
                status: raw_draw.status,
            });
        }

        Ok(Self {
            workspace_name,
            cities,
            campaigns,
            webhook_endpoints,
            event_sources,
            reward_rules,
            events,
            admission_pools,
            reward_draws,
        })
    }
}

impl fmt::Debug for BootstrapSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapSpec")
            .field("workspace_name", &"[REDACTED]")
            .field("city_count", &self.cities.len())
            .field("campaign_count", &self.campaigns.len())
            .field(
                "smart_link_count",
                &self
                    .campaigns
                    .iter()
                    .map(|campaign| campaign.smart_links.len())
                    .sum::<usize>(),
            )
            .field("webhook_endpoint_count", &self.webhook_endpoints.len())
            .field("event_source_count", &self.event_sources.len())
            .field("reward_rule_count", &self.reward_rules.len())
            .field("event_count", &self.events.len())
            .field("reward_draw_count", &self.reward_draws.len())
            .finish()
    }
}

/// Sanitized validation errors. They never include input values.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum BootstrapSpecError {
    /// The JSON document exceeded the maximum allowed size.
    #[error("bootstrap JSON exceeds the {max_bytes}-byte limit")]
    DocumentTooLarge { max_bytes: usize },

    /// The JSON document was malformed or did not match the expected schema.
    #[error("bootstrap JSON is malformed or does not match the expected schema")]
    InvalidJson,

    /// A collection exceeded its maximum number of entries.
    #[error("bootstrap collection {collection} exceeds its limit of {max}")]
    TooMany {
        collection: &'static str,
        max: usize,
    },

    /// A field failed validation.
    #[error("bootstrap field {field} is invalid")]
    InvalidField { field: &'static str },

    /// A field required HTTPS in production but used HTTP.
    #[error("bootstrap field {field} must use HTTPS in production")]
    HttpsRequired { field: &'static str },

    /// A duplicate entry was found in the document.
    #[error("bootstrap document contains a duplicate {kind}")]
    Duplicate { kind: &'static str },
}

/// Per-table rows inserted or materially updated by one bootstrap transaction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootstrapChanges {
    /// Number of workspace rows inserted or updated.
    pub workspaces: u64,
    /// Number of city rows inserted or updated.
    pub cities: u64,
    /// Number of city aggregate rows inserted or updated.
    pub city_aggregates: u64,
    /// Number of campaign rows inserted or updated.
    pub campaigns: u64,
    /// Number of smart-link rows inserted or updated.
    pub smart_links: u64,
    /// Number of webhook endpoint rows inserted or updated.
    pub webhook_endpoints: u64,
    /// Number of external event source rows inserted or updated.
    pub event_sources: u64,
    /// Number of reward rule rows inserted or updated.
    pub reward_rules: u64,
    /// Number of event rows inserted or updated.
    pub events: u64,
    /// Number of admission pool rows inserted or updated.
    pub admission_pools: u64,
    /// Number of weighted reward draw rows inserted or updated.
    pub reward_draws: u64,
}

impl BootstrapChanges {
    /// Returns `true` if no rows were inserted or updated.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.workspaces == 0
            && self.cities == 0
            && self.city_aggregates == 0
            && self.campaigns == 0
            && self.smart_links == 0
            && self.webhook_endpoints == 0
            && self.event_sources == 0
            && self.reward_rules == 0
            && self.events == 0
            && self.admission_pools == 0
            && self.reward_draws == 0
    }

    /// Returns the total number of rows inserted or updated.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.workspaces
            .saturating_add(self.cities)
            .saturating_add(self.city_aggregates)
            .saturating_add(self.campaigns)
            .saturating_add(self.smart_links)
            .saturating_add(self.webhook_endpoints)
            .saturating_add(self.event_sources)
            .saturating_add(self.reward_rules)
            .saturating_add(self.events)
            .saturating_add(self.admission_pools)
            .saturating_add(self.reward_draws)
    }
}

/// Outcome of a committed bootstrap transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapResult {
    /// ID of the bootstrapped workspace.
    pub workspace_id: WorkspaceId,
    /// Per-table row counts.
    pub changes: BootstrapChanges,
    /// Whether the service audit record was committed.
    pub audit_recorded: bool,
}

/// Sanitized runtime errors. SQL errors are intentionally not embedded because
/// database diagnostics can echo constrained input values.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BootstrapError {
    /// The database timeout configuration was invalid.
    #[error("bootstrap database timeout configuration is invalid")]
    InvalidDatabaseTimeouts,

    /// The bootstrap transaction exceeded its operation deadline.
    #[error("workspace bootstrap exceeded its operation deadline")]
    TimedOut,

    /// A database operation failed.
    #[error("workspace bootstrap database operation failed")]
    Database,

    /// A campaign name conflicted with an existing campaign in the workspace.
    #[error("campaign name is not a unique identity in the workspace")]
    CampaignIdentityConflict,

    /// A webhook secret reference differed from the existing value.
    #[error("webhook secret reference differs; use the dedicated rotation flow")]
    WebhookSecretReferenceConflict,
}

/// Applies a validated bootstrap document under a workspace-scoped advisory
/// lock. All data changes and the service audit record commit atomically.
pub async fn bootstrap(
    pool: &PgPool,
    workspace_slug: &WorkspaceSlug,
    database: &DatabaseConfig,
    spec: &BootstrapSpec,
) -> Result<BootstrapResult, BootstrapError> {
    validate_database_timeouts(database)?;
    timeout(
        database.operation_timeout,
        bootstrap_inner(pool, workspace_slug, database, spec),
    )
    .await
    .map_err(|_| BootstrapError::TimedOut)?
}

async fn bootstrap_inner(
    pool: &PgPool,
    workspace_slug: &WorkspaceSlug,
    database: &DatabaseConfig,
    spec: &BootstrapSpec,
) -> Result<BootstrapResult, BootstrapError> {
    let mut transaction = pool.begin().await.map_err(|_| BootstrapError::Database)?;
    configure_transaction(&mut transaction, database).await?;
    acquire_workspace_lock(&mut transaction, workspace_slug).await?;

    let mut changes = BootstrapChanges::default();
    let (workspace_id, workspace_changed) =
        upsert_workspace(&mut transaction, workspace_slug, &spec.workspace_name).await?;
    changes.workspaces = u64::from(workspace_changed);

    for city in &spec.cities {
        let (city_id, city_changed) = upsert_city(&mut transaction, city).await?;
        changes.cities = changes.cities.saturating_add(u64::from(city_changed));
        let aggregate_changed =
            ensure_city_aggregate(&mut transaction, workspace_id, city_id).await?;
        changes.city_aggregates = changes
            .city_aggregates
            .saturating_add(u64::from(aggregate_changed));
    }

    for campaign in &spec.campaigns {
        let (campaign_id, campaign_changed) =
            upsert_campaign(&mut transaction, workspace_id, campaign).await?;
        changes.campaigns = changes
            .campaigns
            .saturating_add(u64::from(campaign_changed));
        for smart_link in &campaign.smart_links {
            let changed =
                upsert_smart_link(&mut transaction, workspace_id, campaign_id, smart_link).await?;
            changes.smart_links = changes.smart_links.saturating_add(u64::from(changed));
        }
    }

    for endpoint in &spec.webhook_endpoints {
        let changed = upsert_webhook_endpoint(&mut transaction, workspace_id, endpoint).await?;
        changes.webhook_endpoints = changes.webhook_endpoints.saturating_add(u64::from(changed));
    }

    for source in &spec.event_sources {
        let changed = upsert_event_source(&mut transaction, workspace_id, source).await?;
        changes.event_sources = changes.event_sources.saturating_add(u64::from(changed));
    }

    for rule in &spec.reward_rules {
        let changed = upsert_reward_rule(&mut transaction, workspace_id, rule).await?;
        changes.reward_rules = changes.reward_rules.saturating_add(u64::from(changed));
    }

    for event in &spec.events {
        let changed = upsert_event(&mut transaction, workspace_id, event).await?;
        changes.events = changes.events.saturating_add(u64::from(changed));
    }

    for pool in &spec.admission_pools {
        let changed = upsert_admission_pool(&mut transaction, workspace_id, pool).await?;
        changes.admission_pools = changes.admission_pools.saturating_add(u64::from(changed));
    }

    for draw in &spec.reward_draws {
        let changed = upsert_reward_draw(&mut transaction, workspace_id, draw).await?;
        changes.reward_draws = changes.reward_draws.saturating_add(u64::from(changed));
    }

    let audit_recorded = !changes.is_empty();
    if audit_recorded {
        append_service_audit(&mut transaction, workspace_id, changes).await?;
    }

    transaction
        .commit()
        .await
        .map_err(|_| BootstrapError::Database)?;
    Ok(BootstrapResult {
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        changes,
        audit_recorded,
    })
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    database: &DatabaseConfig,
) -> Result<(), BootstrapError> {
    let statement_timeout = duration_milliseconds(database.operation_timeout)?;
    let lock_timeout = duration_milliseconds(database.lock_timeout)?;
    sqlx::query(
        r#"
        SELECT
            set_config('statement_timeout', $1, true),
            set_config('lock_timeout', $2, true)
        "#,
    )
    .bind(format!("{statement_timeout}ms"))
    .bind(format!("{lock_timeout}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    Ok(())
}

async fn acquire_workspace_lock(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_slug: &WorkspaceSlug,
) -> Result<(), BootstrapError> {
    let lock_name = format!("{BOOTSTRAP_LOCK_PREFIX}{}", workspace_slug.as_str());
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_name)
        .execute(&mut **transaction)
        .await
        .map_err(|_| BootstrapError::Database)?;
    Ok(())
}

async fn upsert_workspace(
    transaction: &mut Transaction<'_, Postgres>,
    slug: &WorkspaceSlug,
    name: &str,
) -> Result<(Uuid, bool), BootstrapError> {
    let changed_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO workspaces (slug, name)
        VALUES ($1, $2)
        ON CONFLICT (slug) DO UPDATE
        SET name = EXCLUDED.name
        WHERE workspaces.name IS DISTINCT FROM EXCLUDED.name
        RETURNING id
        "#,
    )
    .bind(slug.as_str())
    .bind(name)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    if let Some(id) = changed_id {
        return Ok((id, true));
    }

    let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR UPDATE")
        .bind(slug.as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(|_| BootstrapError::Database)?;
    Ok((id, false))
}

async fn upsert_city(
    transaction: &mut Transaction<'_, Postgres>,
    city: &CitySpec,
) -> Result<(Uuid, bool), BootstrapError> {
    let changed_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO cities (
            slug,
            name,
            country_code,
            region,
            latitude,
            longitude
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (country_code, slug) DO UPDATE
        SET
            name = EXCLUDED.name,
            region = EXCLUDED.region,
            latitude = EXCLUDED.latitude,
            longitude = EXCLUDED.longitude
        WHERE ROW(
            cities.name,
            cities.region,
            cities.latitude,
            cities.longitude
        ) IS DISTINCT FROM ROW(
            EXCLUDED.name,
            EXCLUDED.region,
            EXCLUDED.latitude,
            EXCLUDED.longitude
        )
        RETURNING id
        "#,
    )
    .bind(city.slug.as_str())
    .bind(&city.name)
    .bind(city.country.as_str())
    .bind(&city.region)
    .bind(city.latitude)
    .bind(city.longitude)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    if let Some(id) = changed_id {
        return Ok((id, true));
    }

    let id = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM cities
        WHERE country_code = $1
          AND slug = $2
        FOR UPDATE
        "#,
    )
    .bind(city.country.as_str())
    .bind(city.slug.as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    Ok((id, false))
}

async fn ensure_city_aggregate(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    city_id: Uuid,
) -> Result<bool, BootstrapError> {
    let changed = sqlx::query(
        r#"
        INSERT INTO city_aggregates (workspace_id, city_id)
        VALUES ($1, $2)
        ON CONFLICT (workspace_id, city_id) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .execute(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .rows_affected()
        == 1;
    Ok(changed)
}

async fn upsert_campaign(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    campaign: &CampaignSpec,
) -> Result<(Uuid, bool), BootstrapError> {
    let existing = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM campaigns
        WHERE workspace_id = $1
          AND name = $2
        ORDER BY id
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(&campaign.name)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;

    match existing.as_slice() {
        [] => {
            let id = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO campaigns (workspace_id, name, active)
                VALUES ($1, $2, $3)
                RETURNING id
                "#,
            )
            .bind(workspace_id)
            .bind(&campaign.name)
            .bind(campaign.active)
            .fetch_one(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?;
            Ok((id, true))
        }
        [id] => {
            let changed = sqlx::query(
                r#"
                UPDATE campaigns
                SET active = $3
                WHERE workspace_id = $1
                  AND id = $2
                  AND active IS DISTINCT FROM $3
                "#,
            )
            .bind(workspace_id)
            .bind(id)
            .bind(campaign.active)
            .execute(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?
            .rows_affected()
                == 1;
            Ok((*id, changed))
        }
        _ => Err(BootstrapError::CampaignIdentityConflict),
    }
}

async fn upsert_smart_link(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    campaign_id: Uuid,
    smart_link: &SmartLinkSpec,
) -> Result<bool, BootstrapError> {
    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO smart_links (
            workspace_id,
            campaign_id,
            slug,
            destination_url,
            active
        )
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workspace_id, slug) DO UPDATE
        SET
            campaign_id = EXCLUDED.campaign_id,
            destination_url = EXCLUDED.destination_url,
            active = EXCLUDED.active
        WHERE ROW(
            smart_links.campaign_id,
            smart_links.destination_url,
            smart_links.active
        ) IS DISTINCT FROM ROW(
            EXCLUDED.campaign_id,
            EXCLUDED.destination_url,
            EXCLUDED.active
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(smart_link.slug.as_str())
    .bind(smart_link.destination_url.as_str())
    .bind(smart_link.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_webhook_endpoint(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    endpoint: &WebhookEndpointSpec,
) -> Result<bool, BootstrapError> {
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO webhook_endpoints (
            workspace_id,
            name,
            url,
            signing_secret_ref,
            timeout_ms,
            max_attempts,
            active
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (workspace_id, name) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&endpoint.name)
    .bind(endpoint.url.as_str())
    .bind(&endpoint.signing_secret_ref)
    .bind(i32::try_from(endpoint.timeout_ms).map_err(|_| BootstrapError::Database)?)
    .bind(i32::from(endpoint.max_attempts))
    .bind(endpoint.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    if inserted.is_some() {
        return Ok(true);
    }

    let existing = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, signing_secret_ref
        FROM webhook_endpoints
        WHERE workspace_id = $1
          AND name = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(&endpoint.name)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .ok_or(BootstrapError::Database)?;
    if existing.1 != endpoint.signing_secret_ref {
        return Err(BootstrapError::WebhookSecretReferenceConflict);
    }

    let changed = sqlx::query(
        r#"
        UPDATE webhook_endpoints
        SET
            url = $3,
            timeout_ms = $4,
            max_attempts = $5,
            active = $6
        WHERE workspace_id = $1
          AND id = $2
          AND ROW(url, timeout_ms, max_attempts, active)
              IS DISTINCT FROM ROW($3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(existing.0)
    .bind(endpoint.url.as_str())
    .bind(i32::try_from(endpoint.timeout_ms).map_err(|_| BootstrapError::Database)?)
    .bind(i32::from(endpoint.max_attempts))
    .bind(endpoint.active)
    .execute(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .rows_affected()
        == 1;
    Ok(changed)
}

async fn upsert_event_source(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    source: &EventSourceSpec,
) -> Result<bool, BootstrapError> {
    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO event_sources (
            workspace_id, provider, artist_name, app_id, default_country_code,
            timezone, sync_interval_seconds, active, next_sync_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
        ON CONFLICT (workspace_id, provider, artist_name) DO UPDATE SET
            app_id = EXCLUDED.app_id,
            default_country_code = EXCLUDED.default_country_code,
            timezone = EXCLUDED.timezone,
            sync_interval_seconds = EXCLUDED.sync_interval_seconds,
            active = EXCLUDED.active,
            sync_lease_until = NULL,
            sync_lease_owner = NULL,
            next_sync_at = CASE
                WHEN EXCLUDED.active THEN now()
                ELSE event_sources.next_sync_at
            END
        WHERE ROW(
            event_sources.app_id,
            event_sources.default_country_code,
            event_sources.timezone,
            event_sources.sync_interval_seconds,
            event_sources.active
        ) IS DISTINCT FROM ROW(
            EXCLUDED.app_id,
            EXCLUDED.default_country_code,
            EXCLUDED.timezone,
            EXCLUDED.sync_interval_seconds,
            EXCLUDED.active
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&source.provider)
    .bind(&source.artist_name)
    .bind(&source.app_id)
    .bind(source.default_country_code.as_str())
    .bind(&source.timezone)
    .bind(i32::try_from(source.sync_interval_seconds).map_err(|_| BootstrapError::Database)?)
    .bind(source.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_event(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event: &EventSpec,
) -> Result<bool, BootstrapError> {
    let city_id = if let Some(city_slug) = &event.city_slug {
        let city_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT cities.id
            FROM cities
            INNER JOIN city_aggregates
                ON city_aggregates.city_id = cities.id
                AND city_aggregates.workspace_id = $1
            WHERE cities.slug = $2
            ORDER BY cities.id
            LIMIT 1
            "#,
        )
        .bind(workspace_id)
        .bind(city_slug.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|_| BootstrapError::Database)?
        .ok_or(BootstrapError::Database)?;
        Some(city_id)
    } else {
        None
    };

    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO events (
            workspace_id, city_id, slug, title, description, venue, venue_address,
            timezone, starts_at, doors_at, ends_at, ticket_url, listen_url, image_url,
            trailer_url, external_event_url, status, published_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
            $15, $16, $17, CASE WHEN $17 = 'published' THEN now() ELSE NULL END
        )
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            city_id = EXCLUDED.city_id,
            title = EXCLUDED.title,
            description = EXCLUDED.description,
            venue = EXCLUDED.venue,
            venue_address = EXCLUDED.venue_address,
            timezone = EXCLUDED.timezone,
            starts_at = EXCLUDED.starts_at,
            doors_at = EXCLUDED.doors_at,
            ends_at = EXCLUDED.ends_at,
            ticket_url = EXCLUDED.ticket_url,
            listen_url = EXCLUDED.listen_url,
            image_url = EXCLUDED.image_url,
            trailer_url = EXCLUDED.trailer_url,
            external_event_url = EXCLUDED.external_event_url,
            status = EXCLUDED.status,
            published_at = CASE
                WHEN EXCLUDED.status = 'published' THEN COALESCE(events.published_at, now())
                ELSE events.published_at
            END
        WHERE ROW(
            events.city_id, events.title, events.description, events.venue,
            events.venue_address, events.timezone, events.starts_at, events.doors_at,
            events.ends_at, events.ticket_url, events.listen_url, events.image_url,
            events.trailer_url, events.external_event_url, events.status
        ) IS DISTINCT FROM ROW(
            EXCLUDED.city_id, EXCLUDED.title, EXCLUDED.description, EXCLUDED.venue,
            EXCLUDED.venue_address, EXCLUDED.timezone, EXCLUDED.starts_at, EXCLUDED.doors_at,
            EXCLUDED.ends_at, EXCLUDED.ticket_url, EXCLUDED.listen_url, EXCLUDED.image_url,
            EXCLUDED.trailer_url, EXCLUDED.external_event_url, EXCLUDED.status
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(event.slug.as_str())
    .bind(&event.title)
    .bind(&event.description)
    .bind(&event.venue)
    .bind(&event.venue_address)
    .bind(&event.timezone)
    .bind(event.starts_at)
    .bind(event.doors_at)
    .bind(event.ends_at)
    .bind(event.ticket_url.as_ref().map(DestinationUrl::as_str))
    .bind(event.listen_url.as_ref().map(DestinationUrl::as_str))
    .bind(event.image_url.as_ref().map(DestinationUrl::as_str))
    .bind(event.trailer_url.as_ref().map(DestinationUrl::as_str))
    .bind(
        event
            .external_event_url
            .as_ref()
            .map(DestinationUrl::as_str),
    )
    .bind(&event.status)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_admission_pool(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    pool: &AdmissionPoolSpec,
) -> Result<bool, BootstrapError> {
    let event_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM events WHERE workspace_id = $1 AND slug = $2 FOR SHARE",
    )
    .bind(workspace_id)
    .bind(pool.event_slug.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .ok_or(BootstrapError::Database)?;
    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO admission_pools (
            workspace_id, event_id, slug, name, capacity, active
        ) VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (workspace_id, event_id, slug) DO UPDATE SET
            name = EXCLUDED.name,
            capacity = EXCLUDED.capacity,
            active = EXCLUDED.active
        WHERE ROW(admission_pools.name, admission_pools.capacity, admission_pools.active)
            IS DISTINCT FROM ROW(EXCLUDED.name, EXCLUDED.capacity, EXCLUDED.active)
          AND admission_pools.issued_count + admission_pools.reserved_count <= EXCLUDED.capacity
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(event_id)
    .bind(pool.slug.as_str())
    .bind(&pool.name)
    .bind(i32::try_from(pool.capacity).map_err(|_| BootstrapError::Database)?)
    .bind(pool.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_reward_rule(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    rule: &RewardRuleSpec,
) -> Result<bool, BootstrapError> {
    let (reward_type, config) = match &rule.config {
        RewardRuleConfig::MerchDiscount {
            discount_percent,
            code_prefix,
        } => (
            "merch_discount",
            json!({
                "discount_percent": discount_percent,
                "expires_days": rule.expires_days,
                "code_prefix": code_prefix,
            }),
        ),
        RewardRuleConfig::PhysicalItem { item_name, sku } => (
            "physical_item",
            json!({
                "item_name": item_name,
                "sku": sku,
                "expires_days": rule.expires_days,
            }),
        ),
    };
    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO reward_rules (
            workspace_id,
            name,
            reward_type,
            threshold,
            config,
            active,
            version
        )
        VALUES ($1, $2, $3, $4, $5, $6, 1)
        ON CONFLICT (workspace_id, name) DO UPDATE
        SET
            reward_type = EXCLUDED.reward_type,
            threshold = EXCLUDED.threshold,
            config = EXCLUDED.config,
            active = EXCLUDED.active,
            version = reward_rules.version + 1
        WHERE ROW(
            reward_rules.reward_type,
            reward_rules.threshold,
            reward_rules.config,
            reward_rules.active
        ) IS DISTINCT FROM ROW(
            EXCLUDED.reward_type,
            EXCLUDED.threshold,
            EXCLUDED.config,
            EXCLUDED.active
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&rule.name)
    .bind(reward_type)
    .bind(
        rule.threshold
            .map(i32::try_from)
            .transpose()
            .map_err(|_| BootstrapError::Database)?,
    )
    .bind(config)
    .bind(rule.active)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn upsert_reward_draw(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    draw: &RewardDrawSpec,
) -> Result<bool, BootstrapError> {
    let event_id = if let Some(event_slug) = &draw.event_slug {
        Some(
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM events WHERE workspace_id = $1 AND slug = $2 FOR SHARE",
            )
            .bind(workspace_id)
            .bind(event_slug.as_str())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?
            .ok_or(BootstrapError::Database)?,
        )
    } else {
        None
    };

    let admission_pool_id = if let Some(pool_slug) = &draw.admission_pool_slug {
        let resolved_event_id = event_id.ok_or(BootstrapError::Database)?;
        Some(
            sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id
                FROM admission_pools
                WHERE workspace_id = $1 AND event_id = $2 AND slug = $3
                FOR SHARE
                "#,
            )
            .bind(workspace_id)
            .bind(resolved_event_id)
            .bind(pool_slug.as_str())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?
            .ok_or(BootstrapError::Database)?,
        )
    } else {
        None
    };

    let reward_rule_id = if let Some(rule_name) = &draw.reward_rule_name {
        Some(
            sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id
                FROM reward_rules
                WHERE workspace_id = $1
                  AND name = $2
                  AND reward_type = 'physical_item'
                  AND active
                FOR SHARE
                "#,
            )
            .bind(workspace_id)
            .bind(rule_name)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|_| BootstrapError::Database)?
            .ok_or(BootstrapError::Database)?,
        )
    } else {
        None
    };

    let changed = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO reward_draws (
            workspace_id, slug, name, prize_kind, eligibility_kind,
            event_id, admission_pool_id, reward_rule_id, winner_count,
            base_entries, entries_per_referral, entries_per_checkin, max_entries,
            claim_expires_hours, opens_at, closes_at, draw_at, status
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9,
            $10, $11, $12, $13, $14, $15, $16, $17, $18
        )
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            name = EXCLUDED.name,
            prize_kind = EXCLUDED.prize_kind,
            eligibility_kind = EXCLUDED.eligibility_kind,
            event_id = EXCLUDED.event_id,
            admission_pool_id = EXCLUDED.admission_pool_id,
            reward_rule_id = EXCLUDED.reward_rule_id,
            winner_count = EXCLUDED.winner_count,
            base_entries = EXCLUDED.base_entries,
            entries_per_referral = EXCLUDED.entries_per_referral,
            entries_per_checkin = EXCLUDED.entries_per_checkin,
            max_entries = EXCLUDED.max_entries,
            claim_expires_hours = EXCLUDED.claim_expires_hours,
            opens_at = EXCLUDED.opens_at,
            closes_at = EXCLUDED.closes_at,
            draw_at = EXCLUDED.draw_at,
            status = EXCLUDED.status,
            attempts = 0,
            last_error = NULL,
            completed_at = NULL
        WHERE reward_draws.status IN ('draft', 'scheduled')
          AND ROW(
              reward_draws.name,
              reward_draws.prize_kind,
              reward_draws.eligibility_kind,
              reward_draws.event_id,
              reward_draws.admission_pool_id,
              reward_draws.reward_rule_id,
              reward_draws.winner_count,
              reward_draws.base_entries,
              reward_draws.entries_per_referral,
              reward_draws.entries_per_checkin,
              reward_draws.max_entries,
              reward_draws.claim_expires_hours,
              reward_draws.opens_at,
              reward_draws.closes_at,
              reward_draws.draw_at,
              reward_draws.status
          ) IS DISTINCT FROM ROW(
              EXCLUDED.name,
              EXCLUDED.prize_kind,
              EXCLUDED.eligibility_kind,
              EXCLUDED.event_id,
              EXCLUDED.admission_pool_id,
              EXCLUDED.reward_rule_id,
              EXCLUDED.winner_count,
              EXCLUDED.base_entries,
              EXCLUDED.entries_per_referral,
              EXCLUDED.entries_per_checkin,
              EXCLUDED.max_entries,
              EXCLUDED.claim_expires_hours,
              EXCLUDED.opens_at,
              EXCLUDED.closes_at,
              EXCLUDED.draw_at,
              EXCLUDED.status
          )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(draw.slug.as_str())
    .bind(&draw.name)
    .bind(&draw.prize_kind)
    .bind(&draw.eligibility_kind)
    .bind(event_id)
    .bind(admission_pool_id)
    .bind(reward_rule_id)
    .bind(i32::try_from(draw.winner_count).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.base_entries).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.entries_per_referral).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.entries_per_checkin).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.max_entries).map_err(|_| BootstrapError::Database)?)
    .bind(i32::try_from(draw.claim_expires_hours).map_err(|_| BootstrapError::Database)?)
    .bind(draw.opens_at)
    .bind(draw.closes_at)
    .bind(draw.draw_at)
    .bind(&draw.status)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?
    .is_some();
    Ok(changed)
}

async fn append_service_audit(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    changes: BootstrapChanges,
) -> Result<(), BootstrapError> {
    let metadata = json!({
        "bootstrap_version": 1,
        "changed_rows": {
            "workspaces": changes.workspaces,
            "cities": changes.cities,
            "city_aggregates": changes.city_aggregates,
            "campaigns": changes.campaigns,
            "smart_links": changes.smart_links,
            "webhook_endpoints": changes.webhook_endpoints,
            "event_sources": changes.event_sources,
            "reward_rules": changes.reward_rules,
            "events": changes.events,
            "admission_pools": changes.admission_pools,
            "reward_draws": changes.reward_draws,
            "total": changes.total(),
        }
    });
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id,
            actor_kind,
            action,
            target_type,
            target_id,
            metadata
        )
        VALUES ($1, 'service', $2, 'workspace', $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(AUDIT_ACTION)
    .bind(workspace_id.to_string())
    .bind(metadata)
    .execute(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    Ok(())
}

fn validate_database_timeouts(database: &DatabaseConfig) -> Result<(), BootstrapError> {
    if database.operation_timeout.is_zero()
        || database.lock_timeout.is_zero()
        || database.lock_timeout > database.operation_timeout
    {
        return Err(BootstrapError::InvalidDatabaseTimeouts);
    }
    duration_milliseconds(database.operation_timeout)?;
    duration_milliseconds(database.lock_timeout)?;
    Ok(())
}

fn duration_milliseconds(duration: Duration) -> Result<u64, BootstrapError> {
    let milliseconds =
        u64::try_from(duration.as_millis()).map_err(|_| BootstrapError::InvalidDatabaseTimeouts)?;
    if milliseconds == 0 {
        return Err(BootstrapError::InvalidDatabaseTimeouts);
    }
    Ok(milliseconds)
}

fn ensure_count(
    collection: &'static str,
    count: usize,
    max: usize,
) -> Result<(), BootstrapSpecError> {
    if count > max {
        return Err(BootstrapSpecError::TooMany { collection, max });
    }
    Ok(())
}

fn validate_name(value: String, field: &'static str) -> Result<String, BootstrapSpecError> {
    validate_text(value, field, MAX_NAME_BYTES)
}

fn validate_optional_text(
    value: Option<String>,
    field: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, BootstrapSpecError> {
    value
        .map(|value| validate_text(value, field, max_bytes))
        .transpose()
}

fn validate_text(
    value: String,
    field: &'static str,
    max_bytes: usize,
) -> Result<String, BootstrapSpecError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid_field(field));
    }
    Ok(value)
}

fn validate_coordinates(
    latitude: Option<f64>,
    longitude: Option<f64>,
) -> Result<(Option<f64>, Option<f64>), BootstrapSpecError> {
    match (latitude, longitude) {
        (None, None) => Ok((None, None)),
        (Some(latitude), Some(longitude))
            if latitude.is_finite()
                && longitude.is_finite()
                && (-90.0..=90.0).contains(&latitude)
                && (-180.0..=180.0).contains(&longitude) =>
        {
            Ok((Some(latitude), Some(longitude)))
        }
        _ => Err(invalid_field("cities[].lat/lng")),
    }
}

fn parse_rfc3339(value: &str, field: &'static str) -> Result<OffsetDateTime, BootstrapSpecError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| invalid_field(field))
}

fn parse_optional_rfc3339(
    value: Option<&str>,
    field: &'static str,
) -> Result<Option<OffsetDateTime>, BootstrapSpecError> {
    value.map(|value| parse_rfc3339(value, field)).transpose()
}

fn parse_optional_url(
    value: Option<String>,
    production: bool,
    field: &'static str,
) -> Result<Option<DestinationUrl>, BootstrapSpecError> {
    value
        .map(|value| {
            let url = DestinationUrl::parse(value).map_err(|_| invalid_field(field))?;
            ensure_environment_url(&url, production, field)?;
            Ok(url)
        })
        .transpose()
}

fn ensure_environment_url(
    url: &DestinationUrl,
    production: bool,
    field: &'static str,
) -> Result<(), BootstrapSpecError> {
    if production && !url.as_str().starts_with("https://") {
        return Err(BootstrapSpecError::HttpsRequired { field });
    }
    Ok(())
}

fn valid_secret_reference(reference: &str) -> bool {
    (1..=MAX_SECRET_REFERENCE_BYTES).contains(&reference.len())
        && reference.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

const fn invalid_field(field: &'static str) -> BootstrapSpecError {
    BootstrapSpecError::InvalidField { field }
}

const fn duplicate(kind: &'static str) -> BootstrapSpecError {
    BootstrapSpecError::Duplicate { kind }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBootstrapSpec {
    workspace_name: String,
    cities: Vec<RawCitySpec>,
    campaigns: Vec<RawCampaignSpec>,
    webhook_endpoints: Vec<RawWebhookEndpointSpec>,
    #[serde(default)]
    event_sources: Vec<RawEventSourceSpec>,
    #[serde(default)]
    reward_rules: Vec<RawRewardRuleSpec>,
    #[serde(default)]
    events: Vec<RawEventSpec>,
    #[serde(default)]
    admission_pools: Vec<RawAdmissionPoolSpec>,
    #[serde(default)]
    reward_draws: Vec<RawRewardDrawSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCitySpec {
    slug: String,
    name: String,
    country: String,
    region: Option<String>,
    lat: Option<f64>,
    lng: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCampaignSpec {
    name: String,
    active: bool,
    smart_links: Vec<RawSmartLinkSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSmartLinkSpec {
    slug: String,
    destination_url: String,
    active: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWebhookEndpointSpec {
    name: String,
    url: String,
    signing_secret_ref: String,
    timeout_ms: u32,
    max_attempts: u16,
    active: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventSourceSpec {
    #[serde(default = "default_event_source_provider")]
    provider: String,
    artist_name: String,
    app_id: String,
    #[serde(default = "default_event_source_country")]
    default_country_code: String,
    #[serde(default = "default_event_timezone")]
    timezone: String,
    #[serde(default = "default_event_sync_interval_seconds")]
    sync_interval_seconds: u32,
    #[serde(default = "default_true")]
    active: bool,
}

fn default_event_source_provider() -> String {
    "bandsintown".to_owned()
}

fn default_event_source_country() -> String {
    "PL".to_owned()
}

const fn default_event_sync_interval_seconds() -> u32 {
    1_800
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRewardRuleSpec {
    name: String,
    #[serde(default)]
    threshold: Option<u32>,
    #[serde(default = "default_reward_rule_kind")]
    kind: String,
    expires_days: u32,
    active: bool,
    // `merch_discount` fields.
    #[serde(default)]
    discount_percent: Option<f64>,
    #[serde(default)]
    code_prefix: Option<String>,
    // `physical_item` fields.
    #[serde(default)]
    item_name: Option<String>,
    #[serde(default)]
    sku: Option<String>,
}

fn default_reward_rule_kind() -> String {
    "merch_discount".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEventSpec {
    slug: String,
    city_slug: Option<String>,
    title: String,
    description: Option<String>,
    venue: Option<String>,
    venue_address: Option<String>,
    #[serde(default = "default_event_timezone")]
    timezone: String,
    starts_at: String,
    doors_at: Option<String>,
    ends_at: Option<String>,
    ticket_url: Option<String>,
    listen_url: Option<String>,
    image_url: Option<String>,
    trailer_url: Option<String>,
    external_event_url: Option<String>,
    status: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdmissionPoolSpec {
    event_slug: String,
    slug: String,
    name: String,
    capacity: u32,
    #[serde(default = "default_true")]
    active: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRewardDrawSpec {
    slug: String,
    name: String,
    prize_kind: String,
    #[serde(default = "default_draw_eligibility")]
    eligibility_kind: String,
    event_slug: Option<String>,
    admission_pool_slug: Option<String>,
    reward_rule_name: Option<String>,
    winner_count: u32,
    #[serde(default = "default_base_entries")]
    base_entries: u32,
    #[serde(default = "default_entries_per_referral")]
    entries_per_referral: u32,
    #[serde(default)]
    entries_per_checkin: u32,
    #[serde(default = "default_max_entries")]
    max_entries: u32,
    #[serde(default = "default_claim_expires_hours")]
    claim_expires_hours: u32,
    opens_at: String,
    closes_at: String,
    draw_at: String,
    #[serde(default = "default_draw_status")]
    status: String,
}

fn default_draw_eligibility() -> String {
    "all_active".to_owned()
}

const fn default_base_entries() -> u32 {
    1
}

const fn default_entries_per_referral() -> u32 {
    1
}

const fn default_max_entries() -> u32 {
    1_000
}

const fn default_claim_expires_hours() -> u32 {
    168
}

fn default_draw_status() -> String {
    "draft".to_owned()
}

fn default_true() -> bool {
    true
}

#[derive(Clone)]
struct EventSourceSpec {
    provider: String,
    artist_name: String,
    app_id: String,
    default_country_code: CountryCode,
    timezone: String,
    sync_interval_seconds: u32,
    active: bool,
}

#[derive(Clone)]
struct RewardDrawSpec {
    slug: crowdrelay_domain::EventSlug,
    name: String,
    prize_kind: String,
    eligibility_kind: String,
    event_slug: Option<crowdrelay_domain::EventSlug>,
    admission_pool_slug: Option<crowdrelay_domain::EventSlug>,
    reward_rule_name: Option<String>,
    winner_count: u32,
    base_entries: u32,
    entries_per_referral: u32,
    entries_per_checkin: u32,
    max_entries: u32,
    claim_expires_hours: u32,
    opens_at: OffsetDateTime,
    closes_at: OffsetDateTime,
    draw_at: OffsetDateTime,
    status: String,
}

#[derive(Clone)]
struct AdmissionPoolSpec {
    event_slug: crowdrelay_domain::EventSlug,
    slug: crowdrelay_domain::EventSlug,
    name: String,
    capacity: u32,
    active: bool,
}

fn default_event_timezone() -> String {
    "Europe/Warsaw".to_owned()
}

#[derive(Clone)]
struct EventSpec {
    slug: crowdrelay_domain::EventSlug,
    city_slug: Option<crowdrelay_domain::CitySlug>,
    title: String,
    description: Option<String>,
    venue: Option<String>,
    venue_address: Option<String>,
    timezone: String,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
    ticket_url: Option<DestinationUrl>,
    listen_url: Option<DestinationUrl>,
    image_url: Option<DestinationUrl>,
    trailer_url: Option<DestinationUrl>,
    external_event_url: Option<DestinationUrl>,
    status: String,
}

#[derive(Clone)]
struct RewardRuleSpec {
    name: String,
    threshold: Option<u32>,
    expires_days: u32,
    active: bool,
    config: RewardRuleConfig,
}

/// Reward payload for one `reward_rules` row. A non-null threshold enables
/// deterministic referral fulfillment. Physical-item rules may omit the
/// threshold and act only as fulfillment definitions referenced by weighted
/// draws; shipping remains outside CrowdRelay through n8n.
#[derive(Clone)]
enum RewardRuleConfig {
    MerchDiscount {
        discount_percent: f64,
        code_prefix: String,
    },
    PhysicalItem {
        item_name: String,
        sku: String,
    },
}

#[derive(Clone)]
struct CitySpec {
    slug: CitySlug,
    name: String,
    country: CountryCode,
    region: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

#[derive(Clone)]
struct CampaignSpec {
    name: String,
    active: bool,
    smart_links: Vec<SmartLinkSpec>,
}

#[derive(Clone)]
struct SmartLinkSpec {
    slug: SmartLinkSlug,
    destination_url: DestinationUrl,
    active: bool,
}

#[derive(Clone)]
struct WebhookEndpointSpec {
    name: String,
    url: DestinationUrl,
    signing_secret_ref: String,
    timeout_ms: u32,
    max_attempts: u16,
    active: bool,
}

/// Creates or refreshes the configured administrator and gate-service identities.
pub async fn bootstrap_admission_access(
    pool: &PgPool,
    workspace_slug: &WorkspaceSlug,
    database: &DatabaseConfig,
    admin_email: &str,
    staff_email: &str,
    admin_session_hash: Option<[u8; 32]>,
    staff_session_hash: Option<[u8; 32]>,
) -> Result<(), BootstrapError> {
    validate_database_timeouts(database)?;
    timeout(database.operation_timeout, async {
        let mut transaction = pool.begin().await.map_err(|_| BootstrapError::Database)?;
        configure_transaction(&mut transaction, database).await?;
        acquire_workspace_lock(&mut transaction, workspace_slug).await?;
        let workspace_id =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
                .bind(workspace_slug.as_str())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|_| BootstrapError::Database)?
                .ok_or(BootstrapError::Database)?;
        upsert_service_member(
            &mut transaction,
            workspace_id,
            admin_email,
            "admin",
            admin_session_hash,
        )
        .await?;
        upsert_service_member(
            &mut transaction,
            workspace_id,
            staff_email,
            "staff",
            staff_session_hash,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|_| BootstrapError::Database)?;
        Ok(())
    })
    .await
    .map_err(|_| BootstrapError::TimedOut)?
}

async fn upsert_service_member(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    email: &str,
    role: &str,
    session_hash: Option<[u8; 32]>,
) -> Result<(), BootstrapError> {
    let member_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status)
        VALUES ($1, $2, $3, $4, 'active')
        ON CONFLICT (workspace_id, normalized_email) DO UPDATE SET
            display_name = EXCLUDED.display_name, role = EXCLUDED.role, status = 'active'
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(email)
    .bind(if role == "admin" {
        "CrowdRelay Admin"
    } else {
        "Virya Gate"
    })
    .bind(role)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| BootstrapError::Database)?;
    if let Some(session_hash) = session_hash {
        let csrf_hash: [u8; 32] =
            sha2::Sha256::digest([b"csrf:".as_slice(), session_hash.as_slice()].concat()).into();
        sqlx::query(
            r#"
            INSERT INTO workspace_member_sessions (
                workspace_id, member_id, session_token_hash, csrf_token_hash, expires_at
            ) VALUES ($1, $2, $3, $4, now() + interval '10 years')
            ON CONFLICT (session_token_hash) DO UPDATE SET
                workspace_id = EXCLUDED.workspace_id, member_id = EXCLUDED.member_id,
                csrf_token_hash = EXCLUDED.csrf_token_hash, last_seen_at = now(),
                expires_at = EXCLUDED.expires_at, revoked_at = NULL
            "#,
        )
        .bind(workspace_id)
        .bind(member_id)
        .bind(session_hash.as_slice())
        .bind(csrf_hash.as_slice())
        .execute(&mut **transaction)
        .await
        .map_err(|_| BootstrapError::Database)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{
        "workspace_name": "Example Artist",
        "cities": [{
            "slug": "wroclaw",
            "name": "Wrocław",
            "country": "PL",
            "region": "Dolnośląskie",
            "lat": 51.1079,
            "lng": 17.0385
        }],
        "campaigns": [{
            "name": "Virya launch",
            "active": true,
            "smart_links": [{
                "slug": "listen",
                "destination_url": "https://virya.example/listen",
                "active": true
            }]
        }],
        "webhook_endpoints": [{
            "name": "automation",
            "url": "https://automation.example/hooks/crowdrelay",
            "signing_secret_ref": "docker:/run/secrets/crowdrelay-webhook",
            "timeout_ms": 10000,
            "max_attempts": 12,
            "active": true
        }]
    }"#;

    #[test]
    fn parses_a_complete_production_document() -> Result<(), Box<dyn std::error::Error>> {
        let spec = BootstrapSpec::parse(VALID, true)?;

        assert_eq!(spec.workspace_name, "Example Artist");
        assert_eq!(spec.cities.len(), 1);
        assert_eq!(spec.campaigns.len(), 1);
        assert_eq!(spec.webhook_endpoints.len(), 1);
        Ok(())
    }

    #[test]
    fn merch_discount_reward_rule_defaults_kind_for_backward_compatibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "3 qualified fans = 10% merch",
                "threshold": 3,
                "discount_percent": 10.0,
                "expires_days": 30,
                "code_prefix": "VIRYA",
                "active": true
            }]
        }"#;

        let spec = BootstrapSpec::parse(document, true)?;
        assert_eq!(spec.reward_rules.len(), 1);
        assert!(matches!(
            spec.reward_rules[0].config,
            RewardRuleConfig::MerchDiscount { .. }
        ));
        Ok(())
    }

    #[test]
    fn parses_a_physical_item_reward_rule() -> Result<(), Box<dyn std::error::Error>> {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "5 qualified fans = free album",
                "threshold": 5,
                "kind": "physical_item",
                "item_name": "Virya — Signal (CD)",
                "sku": "virya-signal-cd",
                "expires_days": 60,
                "active": true
            }]
        }"#;

        let spec = BootstrapSpec::parse(document, true)?;
        assert_eq!(spec.reward_rules.len(), 1);
        match &spec.reward_rules[0].config {
            RewardRuleConfig::PhysicalItem { item_name, sku } => {
                assert_eq!(item_name, "Virya — Signal (CD)");
                assert_eq!(sku, "virya-signal-cd");
            }
            RewardRuleConfig::MerchDiscount { .. } => panic!("expected a physical_item reward"),
        }
        Ok(())
    }

    #[test]
    fn rejects_physical_item_reward_rule_missing_required_fields() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "5 qualified fans = free album",
                "threshold": 5,
                "kind": "physical_item",
                "sku": "virya-signal-cd",
                "expires_days": 60,
                "active": true
            }]
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("missing item_name must fail"),
            BootstrapSpecError::InvalidField {
                field: "reward_rules[].item_name"
            }
        );
    }

    #[test]
    fn rejects_reward_rule_with_fields_from_the_wrong_kind() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "5 qualified fans = free album",
                "threshold": 5,
                "kind": "physical_item",
                "item_name": "Virya — Signal (CD)",
                "sku": "virya-signal-cd",
                "discount_percent": 10.0,
                "expires_days": 60,
                "active": true
            }]
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("mismatched kind fields must fail"),
            BootstrapSpecError::InvalidField {
                field: "reward_rules[].kind"
            }
        );
    }

    #[test]
    fn rejects_unknown_reward_rule_kind() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "reward_rules": [{
                "name": "Unknown reward",
                "threshold": 5,
                "kind": "vinyl_time_machine",
                "expires_days": 60,
                "active": true
            }]
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("unknown kind must fail"),
            BootstrapSpecError::InvalidField {
                field: "reward_rules[].kind"
            }
        );
    }

    #[test]
    fn parses_and_validates_event_bootstrap_data() -> Result<(), Box<dyn std::error::Error>> {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [{
                "slug": "wroclaw",
                "name": "Wrocław",
                "country": "PL",
                "region": "Dolnośląskie",
                "lat": 51.1079,
                "lng": 17.0385
            }],
            "campaigns": [],
            "webhook_endpoints": [],
            "events": [{
                "slug": "virya-wroclaw-2027",
                "city_slug": "wroclaw",
                "title": "Virya — Wrocław",
                "description": "Viryatkowo live",
                "venue": "Example Club",
                "venue_address": "Example 1, Wrocław",
                "timezone": "Europe/Warsaw",
                "starts_at": "2027-06-20T18:00:00Z",
                "doors_at": "2027-06-20T17:00:00Z",
                "ends_at": "2027-06-20T21:00:00Z",
                "ticket_url": "https://virya.music/tickets/example",
                "listen_url": "https://virya.music/listen",
                "image_url": "https://virya.music/example.jpg",
                "trailer_url": "https://virya.music/example.mp4",
                "external_event_url": "https://virya.music/live/virya-wroclaw-2027",
                "status": "published"
            }]
        }"#;

        let spec = BootstrapSpec::parse(document, true)?;
        assert_eq!(spec.events.len(), 1);
        Ok(())
    }

    #[test]
    fn parses_manual_admission_pool_for_a_published_event() -> Result<(), Box<dyn std::error::Error>>
    {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [{
                "slug": "example-city",
                "name": "Example City",
                "country": "PL",
                "region": "Opolskie",
                "lat": null,
                "lng": null
            }],
            "campaigns": [],
            "webhook_endpoints": [],
            "events": [{
                "slug": "example-show-2030",
                "city_slug": "example-city",
                "title": "Example Tour",
                "description": null,
                "venue": "Example Venue",
                "venue_address": "123 Example Street",
                "timezone": "Europe/Warsaw",
                "starts_at": "2030-09-05T19:30:00+02:00",
                "doors_at": null,
                "ends_at": null,
                "ticket_url": null,
                "listen_url": null,
                "image_url": null,
                "trailer_url": null,
                "external_event_url": "https://example.test/event",
                "status": "published"
            }],
            "admission_pools": [{
                "event_slug": "example-show-2030",
                "slug": "example-guest-list",
                "name": "Example guest list",
                "capacity": 4,
                "active": true
            }]
        }"#;

        let spec = BootstrapSpec::parse(document, true)?;
        assert_eq!(spec.events.len(), 1);
        assert_eq!(spec.admission_pools.len(), 1);
        assert_eq!(spec.admission_pools[0].capacity, 4);
        assert!(spec.admission_pools[0].active);
        Ok(())
    }

    #[test]
    fn rejects_zero_capacity_admission_pool() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [],
            "webhook_endpoints": [],
            "events": [],
            "admission_pools": [{
                "event_slug": "example-show-2030",
                "slug": "example-guest-list",
                "name": "Example guest list",
                "capacity": 0,
                "active": true
            }]
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("zero capacity must fail"),
            BootstrapSpecError::InvalidField {
                field: "admission_pools[].capacity"
            }
        );
    }

    #[test]
    fn rejects_unknown_fields_at_nested_levels() {
        let document = VALID.replace(
            r#""active": true
            }]"#,
            r#""active": true,
                "raffle": true
            }]"#,
        );

        assert_eq!(
            BootstrapSpec::parse(&document, true).expect_err("unknown field must fail"),
            BootstrapSpecError::InvalidJson
        );
    }

    #[test]
    fn production_requires_https_without_echoing_the_url() {
        let insecure = "http://private.example/secret-path";
        let document = VALID.replace("https://virya.example/listen", insecure);
        let error = BootstrapSpec::parse(&document, true)
            .expect_err("HTTP redirect must be rejected in production");

        assert_eq!(
            error,
            BootstrapSpecError::HttpsRequired {
                field: "campaigns[].smart_links[].destination_url"
            }
        );
        assert!(!format!("{error:?}").contains(insecure));
        assert!(BootstrapSpec::parse(&document, false).is_ok());
    }

    #[test]
    fn rejects_duplicate_smart_link_slugs_across_campaigns() {
        let document = r#"{
            "workspace_name": "Example Artist",
            "cities": [],
            "campaigns": [
                {
                    "name": "First",
                    "active": true,
                    "smart_links": [{
                        "slug": "listen",
                        "destination_url": "https://one.example",
                        "active": true
                    }]
                },
                {
                    "name": "Second",
                    "active": true,
                    "smart_links": [{
                        "slug": "listen",
                        "destination_url": "https://two.example",
                        "active": true
                    }]
                }
            ],
            "webhook_endpoints": []
        }"#;

        assert_eq!(
            BootstrapSpec::parse(document, true).expect_err("duplicate slug must fail"),
            BootstrapSpecError::Duplicate {
                kind: "smart-link slug"
            }
        );
    }

    #[test]
    fn rejects_ambiguous_city_slugs_and_coordinate_halves() {
        let duplicate = r#"{
            "workspace_name": "Example Artist",
            "cities": [
                {
                    "slug": "springfield",
                    "name": "Springfield",
                    "country": "US",
                    "region": null,
                    "lat": null,
                    "lng": null
                },
                {
                    "slug": "springfield",
                    "name": "Springfield",
                    "country": "CA",
                    "region": null,
                    "lat": null,
                    "lng": null
                }
            ],
            "campaigns": [],
            "webhook_endpoints": []
        }"#;
        assert_eq!(
            BootstrapSpec::parse(duplicate, true).expect_err("duplicate city slug must fail"),
            BootstrapSpecError::Duplicate { kind: "city slug" }
        );

        let half = VALID.replace("\"lng\": 17.0385", "\"lng\": null");
        assert_eq!(
            BootstrapSpec::parse(&half, true).expect_err("coordinate half must fail"),
            BootstrapSpecError::InvalidField {
                field: "cities[].lat/lng"
            }
        );
    }

    #[test]
    fn rejects_invalid_secret_references_and_webhook_fragments() {
        let bad_reference = VALID.replace(
            "docker:/run/secrets/crowdrelay-webhook",
            "docker ref with spaces",
        );
        assert_eq!(
            BootstrapSpec::parse(&bad_reference, true).expect_err("bad reference must fail"),
            BootstrapSpecError::InvalidField {
                field: "webhook_endpoints[].signing_secret_ref"
            }
        );

        let fragment = VALID.replace(
            "https://automation.example/hooks/crowdrelay",
            "https://automation.example/hooks/crowdrelay#ignored",
        );
        assert_eq!(
            BootstrapSpec::parse(&fragment, true).expect_err("fragment must fail"),
            BootstrapSpecError::InvalidField {
                field: "webhook_endpoints[].url"
            }
        );
    }

    #[test]
    fn debug_redacts_names_urls_and_secret_references() -> Result<(), Box<dyn std::error::Error>> {
        let spec = BootstrapSpec::parse(VALID, true)?;
        let rendered = format!("{spec:?}");

        assert!(!rendered.contains("Example Artist"));
        assert!(!rendered.contains("virya.example"));
        assert!(!rendered.contains("crowdrelay-webhook"));
        assert!(rendered.contains("smart_link_count"));
        Ok(())
    }

    #[test]
    fn rejects_oversized_documents_before_deserialization() {
        let document = "x".repeat(MAX_DOCUMENT_BYTES + 1);
        assert_eq!(
            BootstrapSpec::parse(&document, false).expect_err("oversized document must fail"),
            BootstrapSpecError::DocumentTooLarge {
                max_bytes: MAX_DOCUMENT_BYTES
            }
        );
    }

    #[test]
    fn change_summary_is_idempotency_friendly() {
        assert!(BootstrapChanges::default().is_empty());
        assert_eq!(BootstrapChanges::default().total(), 0);

        let changes = BootstrapChanges {
            workspaces: 1,
            smart_links: 2,
            ..BootstrapChanges::default()
        };
        assert!(!changes.is_empty());
        assert_eq!(changes.total(), 3);
    }
}
