// Transport-only request DTOs for the ViryaOS operator API.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerBookingPolicyRequest {
    policy: BookingManagerPolicy,
    source: ManagerConfigSource,
    source_revision: Option<String>,
    expected_version: i64,
}

/// The band's vehicles and rates.
///
/// `policy` uses the domain's own `serde(default)` shape, so an operator can
/// send only the fields they are changing and the rest keep their current
/// meaning rather than resetting to zero.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TourEconomicsRequest {
    pub policy: TourEconomicsPolicy,
    pub expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssignActionRequest {
    member_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityRequest {
    enabled: bool,
    autonomy_level: AutonomyLevel,
    minimum_confidence_basis_points: u16,
    max_actions_24h: u32,
    expected_version: i64,
}

/// Whole-envelope write. Every field required: a partial update of a limit set
/// is how one ceiling gets widened while another is believed tightened.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowthEnvelopeRequest {
    pub(super) agent_enabled: bool,
    pub(super) dry_run: bool,
    pub(super) weekly_owned_audience_touches: u32,
    pub(super) weekly_third_party_touches: u32,
    pub(super) subject_cooldown_hours: u32,
    pub(super) max_recipients_per_step: u32,
    pub(super) expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MerchProductEconomicsRequest {
    product_id: Uuid,
    minimum_price_minor: i64,
    maximum_price_minor: i64,
    unit_cost_minor: Option<i64>,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingTargetRequest {
    target_id: Option<Uuid>,
    city_id: Uuid,
    target_kind: BookingTargetKind,
    display_name: String,
    contact_email: String,
    capacity: Option<u32>,
    priority: u16,
    relationship_score: u16,
    active: bool,
    accepts_booking: bool,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TicketAllocationGuardrailRequest {
    ticket_type_id: Uuid,
    minimum_capacity: u32,
    maximum_capacity: u32,
    step_capacity: u32,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionCampaignStateRequest {
    provider: String,
    external_campaign_key: String,
    event_id: Option<Uuid>,
    currency: String,
    current_daily_budget_minor: i64,
    minimum_daily_budget_minor: i64,
    maximum_daily_budget_minor: i64,
    spend_last_7d_minor: i64,
    spend_month_to_date_minor: i64,
    attributed_revenue_last_7d_minor: i64,
    active: bool,
    #[serde(default, with = "time::serde::rfc3339::option")]
    last_budget_change_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionBudgetGuardrailRequest {
    currency: String,
    maximum_total_daily_budget_minor: i64,
    maximum_monthly_spend_minor: i64,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityMarketSignalRequest {
    source: String,
    city_id: Uuid,
    signal_kind: CityMarketSignalKind,
    score_basis_points: u16,
    confidence_basis_points: u16,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookingReplyRequest {
    disposition: BookingReplyDisposition,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeaconRequest {
    beacon_id: Option<Uuid>,
    city_id: Option<Uuid>,
    /// Operator surfaces know a city by the slug the public city list returns.
    /// Accepted as an alternative to `city_id`, never together with it.
    #[serde(default)]
    city_slug: Option<String>,
    beacon_kind: BeaconKind,
    display_name: String,
    contact_email: Option<String>,
    destination_url: Option<String>,
    source_url: Option<String>,
    active: bool,
    verified: bool,
    accepts_outreach: bool,
    do_not_contact: bool,
    relationship_score: u16,
    relevance_basis_points: u16,
    confidence_basis_points: u16,
    #[serde(default)]
    metadata: serde_json::Value,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeaconReplyRequest {
    event_id: Uuid,
    disposition: BeaconReplyDisposition,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachTargetRequest {
    target_id: Option<Uuid>,
    target_kind: OutreachTargetKind,
    display_name: String,
    contact_email: String,
    priority: u16,
    relationship_score: u16,
    active: bool,
    verified: bool,
    accepts_outreach: bool,
    do_not_contact: bool,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachOpportunityRequest {
    opportunity_id: Option<Uuid>,
    target_id: Uuid,
    source: String,
    subject_kind: String,
    subject_key: String,
    template_key: String,
    relevance_basis_points: u16,
    confidence_basis_points: u16,
    active: bool,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachReplyRequest {
    opportunity_id: Option<Uuid>,
    disposition: OutreachReplyDisposition,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePlanRequest {
    release_id: Option<Uuid>,
    source_key: String,
    title: String,
    #[serde(with = "time::serde::rfc3339")]
    release_at: OffsetDateTime,
    listen_url: Option<String>,
    active: bool,
    assets_ready: bool,
    communication_enabled: bool,
    press_enabled: bool,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOpportunityRequest {
    opportunity_id: Option<Uuid>,
    opportunity_kind: TeamOpportunityKind,
    source: String,
    external_key: String,
    title: String,
    organization: String,
    destination_url: Option<String>,
    contact_email: Option<String>,
    verified_destination: bool,
    fit_basis_points: u16,
    reputation_basis_points: u16,
    confidence_basis_points: u16,
    currency: String,
    expected_fee_minor: i64,
    estimated_cost_minor: i64,
    application_fee_minor: i64,
    requires_contract: bool,
    exclusive: bool,
    eligible: bool,
    funding_amount_minor: i64,
    own_contribution_minor: i64,
    #[serde(default, with = "time::serde::rfc3339::option")]
    deadline: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    event_starts_at: Option<OffsetDateTime>,
    country_code: Option<String>,
    travel_band: Option<LiveTravelBand>,
    #[serde(default)]
    metadata: serde_json::Value,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOpportunityProgressRequest {
    progress: TeamOpportunityProgress,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentSourceRequest {
    source_id: Option<Uuid>,
    source_kind: ContentSourceKind,
    source_key: String,
    title: String,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    metadata: serde_json::Value,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentVariantRequest {
    key: String,
    allocation_basis_points: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentRequest {
    slug: String,
    metric: ExperimentMetric,
    variants: Vec<ExperimentVariantRequest>,
    start: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentObservationRequest {
    experiment_id: Uuid,
    variant_id: Uuid,
    exposures_delta: u32,
    conversions_delta: u32,
    value_minor_delta: i64,
    #[serde(with = "time::serde::rfc3339")]
    observed_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentAssignmentRequest {
    assignment_key: String,
}

const PROMOTION_STATE_MAX_TTL: Duration = Duration::hours(24);
const PROMOTION_STATE_CLOCK_SKEW: Duration = Duration::minutes(5);
const MARKET_SIGNAL_MAX_TTL: Duration = Duration::days(7);
