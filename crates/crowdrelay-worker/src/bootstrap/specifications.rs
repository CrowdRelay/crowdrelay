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
    eligibility_ref: Option<String>,
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
    eligibility_ref: Option<crowdrelay_domain::EventSlug>,
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
