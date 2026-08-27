#[derive(Debug, Serialize, FromRow)]
pub struct AudienceOverview {
    active_fans: i64,
    marketing_consented_fans: i64,
    ticket_buyers: i64,
    attendees: i64,
    synesthesia_participants: i64,
    qualified_referrals: i64,
    paid_ticket_orders: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanListQuery {
    limit: Option<i64>,
    search: Option<String>,
    city_slug: Option<String>,
    /// Filter by activation state: "active", "inactive", "inactive_no_consent",
    /// "inactive_never_acted", "inactive_window_expired".
    /// Omit to return all fans regardless of activation.
    activation: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct FanCard {
    id: Uuid,
    email: String,
    display_name: Option<String>,
    locale: Option<String>,
    status: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    qualified_referrals: i64,
    event_interests: i64,
    attended_events: i64,
    paid_ticket_orders: i64,
    synesthesia_entries: i64,
    /// Whether the fan has granted marketing consent.
    consented: bool,
    /// When the fan last did something meaningful, if ever.
    #[serde(with = "time::serde::rfc3339::option")]
    last_activity_at: Option<OffsetDateTime>,
    /// The fan's activation state: "active", "inactive_no_consent",
    /// "inactive_never_acted", "inactive_window_expired", "inactive_account_closed".
    /// Derived from the same definition as the KPI view, not from account status.
    activation_state: String,
}

#[derive(Debug, Serialize, FromRow)]
pub struct AcquisitionTouch {
    source: String,
    campaign_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Serialize, FromRow)]
pub struct EventInterestTouch {
    event_slug: String,
    event_title: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Debug, Serialize, FromRow)]
pub struct AttendanceTouch {
    event_slug: String,
    event_title: String,
    status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    redeemed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct TicketPurchase {
    order_reference: String,
    event_slug: String,
    event_title: String,
    status: String,
    currency: String,
    amount_gross_minor: i64,
    amount_refunded_minor: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    paid_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct RewardTouch {
    reward_name: String,
    reward_type: String,
    status: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Debug, Serialize, FromRow)]
pub struct SynesthesiaTouch {
    campaign_slug: String,
    #[serde(with = "time::serde::rfc3339")]
    entered_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    completed_at: Option<OffsetDateTime>,
    client_total_elapsed_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct FanDetail {
    fan: FanCard,
    acquisitions: Vec<AcquisitionTouch>,
    event_interests: Vec<EventInterestTouch>,
    attendance: Vec<AttendanceTouch>,
    ticket_purchases: Vec<TicketPurchase>,
    rewards: Vec<RewardTouch>,
    synesthesia: Vec<SynesthesiaTouch>,
    tags: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudienceFilter {
    statuses: Vec<String>,
    city_slugs: Vec<String>,
    min_qualified_referrals: Option<i64>,
    interested_event_slugs: Vec<String>,
    attended_event_slugs: Vec<String>,
    purchased_event_slugs: Vec<String>,
    excluded_purchased_event_slugs: Vec<String>,
    synesthesia_completed: Option<bool>,
    marketing_consent: Option<bool>,
    tags_all: Vec<String>,
}

impl AudienceFilter {
    fn validate(&self) -> bool {
        self.statuses.iter().all(|value| {
            matches!(
                value.as_str(),
                "pending" | "active" | "unsubscribed" | "suppressed"
            )
        }) && self.city_slugs.iter().all(|value| valid_slug(value))
            && self
                .interested_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .attended_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .purchased_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .excluded_purchased_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .min_qualified_referrals
                .is_none_or(|value| (0..=1_000_000).contains(&value))
            && self.tags_all.iter().all(|value| valid_tag(value))
            && self.statuses.len() <= 4
            && self.city_slugs.len() <= 50
            && self.interested_event_slugs.len() <= 50
            && self.attended_event_slugs.len() <= 50
            && self.purchased_event_slugs.len() <= 50
            && self.excluded_purchased_event_slugs.len() <= 50
            && self.tags_all.len() <= 50
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSegmentRequest {
    slug: String,
    name: String,
    description: Option<String>,
    #[serde(default)]
    filter: AudienceFilter,
}

#[derive(Debug, Serialize, FromRow)]
pub struct AudienceSegment {
    id: Uuid,
    slug: String,
    name: String,
    description: Option<String>,
    filter: Value,
    active: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewQuery {
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SegmentPreview {
    segment: AudienceSegment,
    total: i64,
    sample: Vec<FanCard>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagRequest {
    tag: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCommunicationCampaignRequest {
    slug: String,
    name: String,
    channel: String,
    segment_slug: String,
    template_key: String,
    subject: Option<String>,
    #[serde(default)]
    content: Value,
}

#[derive(Debug, Serialize, FromRow)]
pub struct CommunicationCampaign {
    id: Uuid,
    slug: String,
    name: String,
    channel: String,
    segment_slug: String,
    template_key: String,
    subject: Option<String>,
    content: Value,
    status: String,
    #[serde(with = "time::serde::rfc3339::option")]
    scheduled_at: Option<OffsetDateTime>,
    dispatch_event_id: Option<Uuid>,
    recipient_count: Option<i32>,
    delivered_count: Option<i32>,
    failed_count: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    completed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    cancelled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleCampaignRequest {
    scheduled_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteCampaignRequest {
    recipient_count: i32,
    delivered_count: i32,
    failed_count: i32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimCampaignDeliveryRequest {
    attempt_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportCampaignDeliveryRequest {
    attempt_key: String,
    status: String,
    provider_reference: Option<String>,
    error_code: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct CampaignDeliveryState {
    fan_id: Uuid,
    attempt_key: String,
    status: String,
    provider_reference: Option<String>,
    error_code: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    claimed_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    completed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct DeliveryProgress {
    eligible_count: i64,
    pending_count: i64,
    claimed_count: i64,
    delivered_count: i64,
    failed_count: i64,
}

#[derive(Debug, Serialize)]
pub struct CampaignDeliveryClaim {
    delivery: CampaignDeliveryState,
    send_allowed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryPlanQuery {
    limit: Option<i64>,
    after_fan_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct DeliveryPlan {
    campaign: CommunicationCampaign,
    recipients: Vec<DeliveryRecipient>,
    next_after_fan_id: Option<Uuid>,
    delivery: DeliveryProgress,
}

#[derive(Debug, Serialize, FromRow)]
pub struct DeliveryRecipient {
    fan_id: Uuid,
    email: String,
    display_name: Option<String>,
    locale: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct FunnelRow {
    source: String,
    acquired_fans: i64,
    active_fans: i64,
    ticket_buyers: i64,
    attendees: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct CityFunnelRow {
    city_slug: String,
    city_name: String,
    country_code: String,
    fans: i64,
    active_30d: i64,
    consented: i64,
    /// Whether this city has enough active fans to be worth booking.
    /// Threshold is 50 active fans — the plan's "two hundred in four cities
    /// produce four shows" implies ~50 per city as the minimum.
    bookable: bool,
}

#[derive(Debug, Serialize, FromRow)]
pub struct ReferralConversionRow {
    /// Total referral attributions (people who used a code).
    referrals_sent: i64,
    /// Attributions that reached 'qualified' status (referred fan qualified).
    qualified: i64,
    /// Qualified referrals whose referred fan is 30d-active (consented +
    /// meaningful action in last 30 days).
    activated: i64,
    /// Qualified referrals that were later reversed (e.g. fan unsubscribed).
    reversed: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct RevenueRow {
    currency: String,
    paid_orders: i64,
    gross_paid_minor: i64,
    refunded_minor: i64,
    after_refunds_minor: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct FanJourneyEntry {
    kind: String,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
    title: String,
    detail: Value,
}

#[derive(Debug, Serialize, FromRow)]
pub struct AdConversionOverviewRow {
    pub attributed_fans: i64,
    pub meta_attributed: i64,
    pub google_attributed: i64,
    pub bandsintown_attributed: i64,
    pub utm_attributed: i64,
    pub meta_lead_delivered: i64,
    pub meta_lead_delivered_ok: i64,
    pub meta_purchase_delivered: i64,
    pub meta_purchase_delivered_ok: i64,
    pub google_lead_delivered: i64,
    pub google_lead_delivered_ok: i64,
    pub google_purchase_delivered: i64,
    pub google_purchase_delivered_ok: i64,
    pub bandsintown_lead_delivered: i64,
    pub bandsintown_lead_delivered_ok: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct AdConversionBreakdownRow {
    pub platform: Option<String>,
    pub event_name: Option<String>,
    pub utm_source: String,
    pub utm_medium: String,
    pub utm_campaign: String,
    pub attributed_fans: i64,
    pub delivered: i64,
    pub delivered_ok: i64,
}
