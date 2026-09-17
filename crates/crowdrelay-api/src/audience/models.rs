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

#[derive(Debug, Serialize, FromRow)]
pub struct AcquisitionSourceRow {
    pub source: String,
    pub fans: i64,
    pub fans_30d: i64,
}

#[derive(Debug, FromRow)]
pub struct AcquisitionTotalsRow {
    pub active_fans: i64,
    pub tracked_fans: i64,
}

#[derive(Debug, Serialize)]
pub struct AcquisitionSources {
    pub active_fans: i64,
    pub tracked_fans: i64,
    pub sources: Vec<AcquisitionSourceRow>,
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
    /// Events whose email-claim check-ins already received the scan welcome.
    /// The welcome is the fan's one contact for the night, so campaigns use
    /// this to keep the next send from double-contacting them.
    excluded_scan_checkin_event_slugs: Vec<String>,
    /// Campaigns whose confirmed-or-claimed deliveries mark a fan as already
    /// reached. The release late waves ("you might have missed it",
    /// catalogue rotation) use this so the second send provably skips every
    /// fan an earlier phase contacted — a `failed` delivery did not reach
    /// them, so it does not exclude.
    excluded_campaign_slugs: Vec<String>,
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
                .excluded_scan_checkin_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .excluded_campaign_slugs
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
            && self.excluded_scan_checkin_event_slugs.len() <= 50
            && self.excluded_campaign_slugs.len() <= 50
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
    region: Option<String>,
    fans: i64,
    /// Fans who declared interest in this city inside the last 30 days —
    /// the trend edge: a small city growing fast reads differently from a
    /// large one standing still.
    new_30d: i64,
    active_30d: i64,
    consented: i64,
    /// Whether this city has enough active fans to be worth booking.
    /// Threshold is 50 active fans — the plan's "two hundred in four cities
    /// produce four shows" implies ~50 per city as the minimum.
    bookable: bool,
    /// Fans the nearby-gig emitter would actually page for a show here:
    /// location preference enabled, account active, marketing consent,
    /// fan's city inside their chosen radius of this one.
    reachable: i64,
    /// Confirmed bookable targets in this city by kind — active with
    /// accepts_booking; the "what's there" inventory beside the fans.
    venues: i64,
    promoters: i64,
    festivals: i64,
    /// Last published show in this city — NULL means never on record.
    #[serde(with = "time::serde::rfc3339::option")]
    last_show_at: Option<OffsetDateTime>,
    /// Next published show in this city — NULL with real fans is the
    /// organise-now gap.
    #[serde(with = "time::serde::rfc3339::option")]
    next_show_at: Option<OffsetDateTime>,
    /// Whole months since `last_show_at`; NULL when never played. Filled
    /// after the query — the organise score's staleness input.
    #[sqlx(default)]
    months_since_show: Option<i64>,
    /// `domain::place::organise_score` in basis points — the ranked
    /// organise-now answer; 0 while a show is already booked forward.
    #[sqlx(default)]
    organise_score_bp: i64,
}

/// One row of the shared venue registry read (§4f-2). A venue is a global
/// object — one room no matter how many tenants marked it — and every
/// number on this row is an aggregate over contributed marks, never a
/// tenant's raw row. `contributors` is a count, not an identity.
///
/// The `*_fact` triples are the resolved claims from `place_venue_facts`:
/// the first non-expired global fact per attribute in provenance trust
/// order, with the provenance that won it and when it was observed. NULL
/// means "not known" — never a zero or an empty string — and the query
/// filters to `workspace_id IS NULL`, so a tenant's private knowledge (a
/// booking address, a fit judgement) can never appear here.
#[derive(Debug, Serialize, FromRow)]
pub struct CityVenueRow {
    venue_id: Uuid,
    display_name: String,
    city_slug: String,
    city_name: String,
    country_code: String,
    /// Shows this room has already seen — published/completed marks whose
    /// start time has passed, across all contributing workspaces.
    shows_played: i64,
    /// Published marks still ahead — nights the room has booked but not
    /// yet hosted. Kept separate from `shows_played`: a future booking is
    /// not a played record.
    shows_booked: i64,
    /// Distinct workspaces that contributed a mark. The only cross-tenant
    /// fact the read exposes — a count, never which tenants.
    contributors: i64,
    /// Mean paid ticket orders per show at this room, over shows that had
    /// a ticket sale at all. An unticketed show's draw is unmeasurable, so
    /// it stays out of the average rather than reading as zero. NULL when
    /// no marked show sold tickets through us.
    typical_draw: Option<f64>,
    /// Buyers (by email) with paid orders at two or more shows at this
    /// room — the room's regulars, and the cross-tenant knowledge a single
    /// band's own history cannot produce.
    repeat_attenders: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    last_played_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    next_show_at: Option<OffsetDateTime>,
    /// How many people fit in the room, as the winning source claims it.
    capacity_fact: Option<String>,
    capacity_provenance: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    capacity_observed_at: Option<OffsetDateTime>,
    /// The room's genres as a ", "-joined tag list — a bias, never a
    /// filter.
    genres_fact: Option<String>,
    genres_provenance: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    genres_observed_at: Option<OffsetDateTime>,
    website_fact: Option<String>,
    website_provenance: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    website_observed_at: Option<OffsetDateTime>,
    address_fact: Option<String>,
    address_provenance: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    address_observed_at: Option<OffsetDateTime>,
    /// Only ever "closed" — the absence of a status fact IS the active
    /// claim, so this row never carries "active" as a value.
    status_fact: Option<String>,
    status_provenance: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    status_observed_at: Option<OffsetDateTime>,
    /// §12-1: the verdict half of the evidence answer — `worth_contact` or
    /// `insufficient_evidence`. Not a column: the registry query knows the
    /// facts, and `venue_evidence::assess` fills this in after the fetch.
    #[sqlx(default)]
    assessment: String,
    /// The one-sentence answer in the tenant's crew locale — the
    /// worth-contact because-list over the strongest facts, or the honest
    /// refusal. Filled after the query, same as `assessment`.
    #[sqlx(default)]
    assessment_sentence: String,
}

/// One tenant-private resolved venue fact. `private_venue_facts` reads the
/// caller's own `booking_email`/`target_fit`/`contact_quality` claims for
/// the listed rooms so the sentence can count a fresh contact as evidence —
/// the private *value* never appears on the shared row, only its age does.
#[derive(Debug, FromRow)]
pub struct PrivateVenueFactRow {
    venue_id: Uuid,
    attribute: String,
    value: String,
    provenance: String,
    observed_at: OffsetDateTime,
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
