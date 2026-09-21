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

/// Approve a parked action, optionally with the operator's corrected words.
///
/// `revision` maps draft fields to replacement text. Which fields may be
/// revised is `draft_revision::REVISABLE_FIELDS` — the pending action's
/// `revisable` map is the same definition, so a client renders the editable
/// surface rather than guessing. An absent or empty body approves as before.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveActionRequest {
    #[serde(default)]
    pub revision: Option<std::collections::BTreeMap<String, String>>,
    /// "…and stop asking me about this target."
    ///
    /// Writes a standing approval for the action's own target once the
    /// approval itself succeeds. Absent means approve this one and ask again
    /// next time, which stays the default: a standing grant is a change of
    /// authority and should be something the operator typed, not something
    /// they got by clicking the usual button.
    #[serde(default)]
    pub remember: Option<RememberRequest>,
}

/// How long "stop asking me" lasts, and why the operator said yes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RememberRequest {
    /// Omitted means `DEFAULT_GRANT_DAYS`. Bounded by `MAX_GRANT_DAYS`.
    #[serde(default)]
    pub days: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Approve several parked actions in one call.
///
/// Explicit ids rather than a filter. "Approve everything in this context" is
/// the shape that makes an approval meaningless — the operator would be
/// granting authority over work they have not read, which is the thing the
/// whole approval queue exists to prevent. Naming the ids means they saw them.
///
/// No revisions here: editing a draft is a per-action act and a batch that
/// silently applied one operator correction to several drafts would be worse
/// than making them do it one at a time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveActionsRequest {
    pub action_ids: Vec<Uuid>,
    #[serde(default)]
    pub remember: Option<RememberRequest>,
}

/// Approve a community relay batch, optionally overriding its cadence and
/// fixing a delivery's words.
///
/// `interval_seconds` is the gap between two posts from the batch — the
/// "one an hour so they don't ban us" the card shows. The table's CHECK
/// (300–86400) is the floor and ceiling; an absent body approves at the
/// batch's own default.
///
/// `revisions` maps a delivery's `action_id` — the id the card shows per
/// target row — to `{field: replacement text}`. Only the post's own words
/// may change (`draft_revision::RELAY_REVISABLE_FIELDS`: `title`, `body`);
/// the community, the link and the media are the batch's facts. A refused
/// edit refuses the whole approval — the batch never approves around a
/// draft the operator meant to fix.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityRelayApproveRequest {
    #[serde(default)]
    pub interval_seconds: Option<i32>,
    #[serde(default)]
    pub revisions:
        Option<std::collections::BTreeMap<Uuid, std::collections::BTreeMap<String, String>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityRequest {
    enabled: bool,
    autonomy_level: AutonomyLevel,
    minimum_confidence_basis_points: u16,
    max_actions_24h: u32,
    expected_version: i64,
    /// Optional domain-policy knobs for this context, validated against the
    /// same typed config the decision reader parses. Absent means "leave the
    /// stored knobs alone"; `{}` means reset to defaults.
    #[serde(default)]
    config: Option<serde_json::Value>,
}

const fn default_daily_third_party_touches() -> u32 {
    3
}

/// The migration's default. An omitted field must not silently mean zero:
/// zero here is "never ask a person about anything", which is a posture a
/// tenant may choose and not one a stale client should choose for them.
const fn default_weekly_approval_requests() -> u32 {
    20
}

/// The migration's default, restated here so a caller that omits the field
/// gets the warm-up rather than silently switching it off. Zero would be the
/// safer-looking number and the wrong one: zero is what the system already
/// did, and it is why the evidence floor was never reached.
const fn default_weekly_bootstrap_actions() -> u32 {
    5
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
    /// Defaults rather than required: a caller that predates the field gets
    /// the safe default instead of a 400 on a limit it never saw.
    #[serde(default = "default_daily_third_party_touches")]
    pub(super) daily_third_party_touches: u32,
    pub(super) subject_cooldown_hours: u32,
    pub(super) max_recipients_per_step: u32,
    /// Defaults rather than required, for the same reason as the daily wall
    /// above: a caller that predates the field gets the default rather than a
    /// 400 on a limit it never saw.
    #[serde(default = "default_weekly_approval_requests")]
    pub(super) weekly_approval_requests: u32,
    /// As above.
    #[serde(default = "default_weekly_bootstrap_actions")]
    pub(super) weekly_bootstrap_actions: u32,
    pub(super) expected_version: i64,
    /// Tenant park flag. When true, the autopilot cycle skips entirely.
    /// Defaults to false for backward compatibility with existing callers
    /// that don't send it.
    #[serde(default)]
    pub(super) parked: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowthPostureRequest {
    /// One of `grounded`, `working`, `full_send`. Anything else is refused
    /// here rather than silently mapped to the nearest safe posture.
    posture: String,
    expected_version: i64,
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

/// One edition of a festival series — the application window the deadline
/// ask and the attention radar both read. Upserted on
/// `(target_id, edition_label)`: correcting a window writes the same row.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FestivalEditionRequest {
    edition_label: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    starts_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    application_opens_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    application_closes_at: Option<OffsetDateTime>,
    lineup_url: Option<String>,
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
    /// The reply's own words, when the operator pasted them in. Present means
    /// the reply joins the triage queue — the deterministic reader proposes
    /// the terms it finds for the human to confirm.
    #[serde(default)]
    reply_text: Option<String>,
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
    #[serde(default)]
    accepts_outreach_basis: Option<String>,
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
    /// The free-text body of the reply, when n8n captured it. When present,
    /// the worker re-classifies it with the first-party domain classifier
    /// rather than trusting the disposition n8n assigned.
    #[serde(default)]
    reply_text: Option<String>,
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
    /// The band's call about what kind of release this is. Absent leaves a
    /// stored tier alone and defaults a new plan to `track`.
    tier: Option<ReleaseTier>,
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
    /// Operator-confirmed, `0..=10_000`. Absent on existing callers defaults to
    /// Standard tier, so a discovery-posted opportunity is never granted
    /// prestige by omission.
    #[serde(default)]
    strategic_value_basis_points: u16,
    /// When the source was actually observed. `None` stays `None` — the row
    /// keeps whatever observation it had rather than borrowing the write time.
    #[serde(default, with = "time::serde::rfc3339::option")]
    source_observed_at: Option<OffsetDateTime>,
    expected_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOpportunityProgressRequest {
    progress: TeamOpportunityProgress,
    #[serde(with = "time::serde::rfc3339")]
    occurred_at: OffsetDateTime,
    /// Required for `lost` and `dismissed`: a row that closes says why, so a
    /// refusal teaches the pipeline instead of disappearing.
    #[serde(default)]
    reason: Option<String>,
}

/// Where the promoter stands. The agent never invents this: somebody read an
/// email and wrote down what it said.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromoterPositionRequest {
    Offer,
    Withdrawn,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOpportunityTermsRequest {
    position: PromoterPositionRequest,
    /// Ignored on a withdrawal, because there is no longer an offer.
    #[serde(default)]
    offered_fee_minor: i64,
    currency: String,
    #[serde(with = "time::serde::rfc3339")]
    responds_by: OffsetDateTime,
}

/// What one report about a placement says.
///
/// `claimed` is the curator's word and counts toward nothing. The other three
/// are what a public read found — and `unreadable` is deliberately its own
/// value, because a dead credential is not evidence that a track is gone.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementReportRequest {
    Claimed,
    Present,
    Absent,
    Unreadable,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistPlacementRequest {
    opportunity_id: Uuid,
    playlist_external_id: String,
    track_external_id: String,
    report: PlacementReportRequest,
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
    /// Omit on create (defaults to active) or to leave the flag alone on
    /// edit; send it to retire or reinstate the source.
    active: Option<bool>,
    /// The catalogue format this artifact was produced in. Omit to leave it
    /// undeclared (create) or unchanged (edit); when present it must name a
    /// `viryaos_content_format_entries` key.
    format_key: Option<String>,
    expected_version: i64,
}

/// The two outcomes an operator may report on an approved suggestion.
/// `declined` and `expired` are not reportable — one is a decision verb on
/// the ask, the other is the sweep's verdict on a window that closed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionOutcomeReportRequest {
    Done,
    DoneDifferently,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportSuggestionOutcomeRequest {
    pub outcome: SuggestionOutcomeReportRequest,
    /// `done_differently` must name what was made instead — "a version of
    /// yes" without the version is a shrug the loop cannot learn from.
    /// Optional on a plain `done`.
    pub reason: Option<String>,
    /// What the band measured already, when it did — `{"views": ...}`.
    /// Stored verbatim for the learning loop; never required.
    pub results: Option<serde_json::Value>,
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
