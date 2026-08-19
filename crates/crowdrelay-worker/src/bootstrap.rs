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
    smart_links: Vec<SmartLinkSpec>,
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
        ensure_count("smart_links", raw.smart_links.len(), MAX_SMART_LINKS)?;
        let mut smart_link_count = raw.smart_links.len();
        let mut smart_links = Vec::with_capacity(raw.smart_links.len());
        for raw_link in raw.smart_links {
            let slug = SmartLinkSlug::parse(&raw_link.slug)
                .map_err(|_| invalid_field("smart_links[].slug"))?;
            if !smart_link_slugs.insert(slug.clone()) {
                return Err(duplicate("smart-link slug"));
            }
            let destination_url = DestinationUrl::parse(&raw_link.destination_url)
                .map_err(|_| invalid_field("smart_links[].destination_url"))?;
            ensure_environment_url(
                &destination_url,
                production,
                "smart_links[].destination_url",
            )?;
            smart_links.push(SmartLinkSpec {
                slug,
                destination_url,
                active: raw_link.active,
            });
        }
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
            let eligibility_ref = raw_draw
                .eligibility_ref
                .map(|value| {
                    crowdrelay_domain::EventSlug::parse(value)
                        .map_err(|_| invalid_field("reward_draws[].eligibility_ref"))
                })
                .transpose()?;
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
                "all_active" | "event_interest" | "synesthesia_completion"
            ) {
                return Err(invalid_field("reward_draws[].eligibility_kind"));
            }
            if raw_draw.eligibility_kind == "event_interest" && event_slug.is_none() {
                return Err(invalid_field("reward_draws[].event_slug"));
            }
            if raw_draw.eligibility_kind == "synesthesia_completion" {
                if eligibility_ref.is_none()
                    || event_slug.is_some()
                    || raw_draw.winner_count != 5
                    || raw_draw.base_entries != 1
                    || raw_draw.entries_per_referral != 0
                    || raw_draw.entries_per_checkin != 0
                    || raw_draw.max_entries != 1
                {
                    return Err(invalid_field("reward_draws[].eligibility_ref"));
                }
            } else if eligibility_ref.is_some() {
                return Err(invalid_field("reward_draws[].eligibility_ref"));
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
                eligibility_ref,
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
            smart_links,
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
                &(self.smart_links.len()
                    + self
                        .campaigns
                        .iter()
                        .map(|campaign| campaign.smart_links.len())
                        .sum::<usize>()),
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

// Applies a validated bootstrap document under a workspace-scoped advisory
// lock. All data changes and the service audit record commit atomically.

// Physical sections compile into this module through `include!`.
// This preserves the established API and item visibility while keeping
// high-risk domains small enough to review and profile independently.
include!("bootstrap/persistence.rs");
include!("bootstrap/validation.rs");
include!("bootstrap/specifications.rs");
include!("bootstrap/admission.rs");
include!("bootstrap/team.rs");
include!("bootstrap/tests.rs");
