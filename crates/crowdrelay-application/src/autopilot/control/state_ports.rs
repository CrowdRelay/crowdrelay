/// Administrative port for Autopilot. Keeping this separate from the evaluator
/// port prevents operator/read-model concerns from leaking into decision code.
#[derive(Clone, Debug)]
pub struct UpsertPromotionCampaignState {
    pub provider: String,
    pub external_campaign_key: String,
    pub event_id: Option<EventId>,
    pub currency: String,
    pub current_daily_budget_minor: i64,
    pub minimum_daily_budget_minor: i64,
    pub maximum_daily_budget_minor: i64,
    pub spend_last_7d_minor: i64,
    pub spend_month_to_date_minor: i64,
    pub attributed_revenue_last_7d_minor: i64,
    pub active: bool,
    pub last_budget_change_at: Option<OffsetDateTime>,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct PromotionCampaignStateMutation {
    pub operation_id: uuid::Uuid,
    pub campaign_id: PromotionCampaignId,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertPromotionBudgetGuardrail {
    pub currency: String,
    pub maximum_total_daily_budget_minor: i64,
    pub maximum_monthly_spend_minor: i64,
    /// `0` creates the guardrail; positive values update exactly that version.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PromotionBudgetGuardrailMutation {
    pub operation_id: uuid::Uuid,
    pub currency: String,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertCityMarketSignal {
    pub source: String,
    pub city_id: CityId,
    pub kind: CityMarketSignalKind,
    pub score_basis_points: u16,
    pub confidence: Confidence,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct CityMarketSignalMutation {
    pub operation_id: uuid::Uuid,
    pub signal_id: MarketSignalId,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertBookingTarget {
    pub target_id: Option<BookingTargetId>,
    pub city_id: CityId,
    pub kind: BookingTargetKind,
    pub display_name: String,
    pub contact_email: String,
    /// Optional verified room/event capacity used only for deterministic fit.
    pub capacity: Option<u32>,
    pub priority: u16,
    pub relationship_score: u16,
    pub active: bool,
    pub accepts_booking: bool,
    /// `0` creates a target; positive values update exactly that target version.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BookingTargetMutation {
    pub operation_id: uuid::Uuid,
    pub target_id: BookingTargetId,
    pub version: i64,
    pub replayed: bool,
}

/// One edition of a festival series — the schedulable object: "Brutal
/// Assault 2027" under the "Brutal Assault" target. Upserted on the natural
/// key `(target_id, edition_label)`: an operator correcting a window writes
/// the same edition, not a sibling row.
#[derive(Clone, Debug)]
pub struct UpsertFestivalEdition {
    pub target_id: BookingTargetId,
    pub edition_label: String,
    /// When the edition runs. `None` is "dates not announced" — an
    /// application window can be known before the weekend is.
    pub starts_at: Option<OffsetDateTime>,
    pub application_opens_at: Option<OffsetDateTime>,
    pub application_closes_at: Option<OffsetDateTime>,
    pub lineup_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FestivalEditionMutation {
    pub operation_id: uuid::Uuid,
    pub edition_id: uuid::Uuid,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct RecordBookingReply {
    pub target_id: BookingTargetId,
    pub disposition: BookingReplyDisposition,
    pub occurred_at: OffsetDateTime,
    /// The reply's own words, when the operator pasted them in. Present means
    /// the reply joins the triage queue — a negotiation reply always needs a
    /// human, and the deterministic reader proposes the terms it finds.
    pub reply_text: Option<String>,
}

/// What a booking agent said back, filed against the registry entity itself
/// (§4h-10). Kept apart from `RecordBookingReply`: a booking target answers
/// for one night, an agent answers for the season — so a decline here stamps
/// `booking_agents.refused_until` and a do-not-contact is the wall,
/// not a cooldown.
#[derive(Clone, Copy, Debug)]
pub struct RecordBookingAgentReply {
    pub agent_id: BookingAgentId,
    pub disposition: BookingAgentReplyDisposition,
    pub occurred_at: OffsetDateTime,
}

/// The booking-agent registry: list, the gated approach request, and the
/// reply ledger. The address never leaves this trait — list rows carry no
/// email, and the approach command names the agent by id so a caller cannot
/// invent a recipient.
#[async_trait]
pub trait AutopilotBookingAgentStateRepository: Send + Sync {
    /// Records the agent's reply. `Declined` stamps the season's
    /// `refused_until`; `DoNotContact` stamps the flag and the contact
    /// governor so every other route honours it too. Idempotent on
    /// `idempotency_key` like every other recorded reply.
    async fn record_booking_agent_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordBookingAgentReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[async_trait]
pub trait AutopilotBookingStateRepository: Send + Sync {
    async fn upsert_booking_target(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertBookingTarget,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<BookingTargetMutation, RepositoryError>;

    /// Registers or corrects one edition's window on a festival target. The
    /// close date is what the deadline ask and the attention radar read —
    /// without this write both are permanently blind.
    async fn upsert_festival_edition(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertFestivalEdition,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<FestivalEditionMutation, RepositoryError>;

    async fn record_booking_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordBookingReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Links a booking target to one of its rooms (§12-5 entity 6). A
    /// promoter works several rooms — the edge is the many beside the
    /// target's single primary `venue_id`. Idempotent by primary key:
    /// linking an already-linked room answers `Ok(false)`, and an unknown
    /// target or venue is `NotFound`, never a silent no-op.
    async fn link_target_venue(
        &self,
        workspace_id: WorkspaceId,
        target_id: BookingTargetId,
        venue_id: VenueId,
    ) -> Result<bool, RepositoryError>;

    /// Removes a promoter↔venue edge. `Ok(true)` when the edge existed;
    /// `Ok(false)` is the idempotent answer for an edge that was already
    /// absent. An unknown target is `NotFound` — a mistyped id must not read
    /// as a successful unlink.
    async fn unlink_target_venue(
        &self,
        workspace_id: WorkspaceId,
        target_id: BookingTargetId,
        venue_id: VenueId,
    ) -> Result<bool, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct UpsertBeacon {
    pub beacon_id: Option<BeaconId>,
    pub city_id: Option<CityId>,
    pub kind: BeaconKind,
    pub display_name: String,
    pub contact_email: Option<String>,
    pub destination_url: Option<String>,
    pub source_url: Option<String>,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub do_not_contact: bool,
    pub relationship_score: u16,
    pub relevance_basis_points: u16,
    pub confidence: Confidence,
    pub metadata: serde_json::Value,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BeaconMutation {
    pub operation_id: uuid::Uuid,
    pub beacon_id: BeaconId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordBeaconReply {
    pub beacon_id: BeaconId,
    pub event_id: EventId,
    pub disposition: BeaconReplyDisposition,
    pub occurred_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotBeaconStateRepository: Send + Sync {
    async fn upsert_beacon(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertBeacon,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<BeaconMutation, RepositoryError>;

    async fn record_beacon_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordBeaconReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Copy, Debug)]
pub struct UpsertTicketAllocationGuardrail {
    pub ticket_type_id: crowdrelay_domain::TicketTypeId,
    pub minimum_capacity: u32,
    pub maximum_capacity: u32,
    pub step_capacity: u32,
    /// `0` creates the guardrail row; positive values update exactly that version.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TicketAllocationGuardrailMutation {
    pub operation_id: uuid::Uuid,
    pub ticket_type_id: crowdrelay_domain::TicketTypeId,
    pub version: i64,
    pub replayed: bool,
}

#[async_trait]
pub trait AutopilotTicketStateRepository: Send + Sync {
    async fn upsert_ticket_allocation_guardrail(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertTicketAllocationGuardrail,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<TicketAllocationGuardrailMutation, RepositoryError>;
}

#[derive(Clone, Copy, Debug)]
pub struct UpsertMerchProductEconomics {
    pub product_id: MerchProductId,
    pub minimum_price_minor: i64,
    pub maximum_price_minor: i64,
    pub unit_cost_minor: Option<i64>,
    /// `0` creates the guardrail row; positive values update exactly that version.
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct MerchProductEconomicsMutation {
    pub operation_id: uuid::Uuid,
    pub product_id: MerchProductId,
    pub version: i64,
    pub replayed: bool,
}

#[async_trait]
pub trait AutopilotMerchStateRepository: Send + Sync {
    async fn upsert_merch_product_economics(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertMerchProductEconomics,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<MerchProductEconomicsMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct UpsertOutreachTarget {
    pub target_id: Option<OutreachTargetId>,
    pub kind: OutreachTargetKind,
    pub display_name: String,
    pub contact_email: String,
    pub priority: u16,
    pub relationship_score: u16,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub do_not_contact: bool,
    /// The stated reason this contact accepts approaches — required when a
    /// representation kind (agent, label) carries `accepts_outreach`; the
    /// schema CHECK refuses the flag without it.
    pub accepts_outreach_basis: Option<String>,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutreachTargetMutation {
    pub operation_id: uuid::Uuid,
    pub target_id: OutreachTargetId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct UpsertOutreachOpportunity {
    pub opportunity_id: Option<OutreachOpportunityId>,
    pub target_id: OutreachTargetId,
    pub source: String,
    pub subject_kind: String,
    pub subject_key: String,
    pub template_key: String,
    pub relevance_basis_points: u16,
    pub confidence: Confidence,
    pub active: bool,
    pub observed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutreachOpportunityMutation {
    pub operation_id: uuid::Uuid,
    pub opportunity_id: OutreachOpportunityId,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct RecordOutreachReply {
    pub target_id: OutreachTargetId,
    pub opportunity_id: Option<OutreachOpportunityId>,
    pub disposition: OutreachReplyDisposition,
    /// The free-text body of the reply, when the caller captured it. When
    /// present, the infra adapter inserts a row into the reply triage queue
    /// so the worker can re-classify it with the first-party classifier.
    pub reply_text: Option<String>,
    pub occurred_at: OffsetDateTime,
}

/// The act wrote to an outreach contact outside the machine — from its own
/// mailbox, usually in answer to a reply. Logged so the contact's
/// conversation shows the act spoke last and leaves "your turn".
#[derive(Clone, Debug)]
pub struct RecordOutreachWritten {
    pub target_id: OutreachTargetId,
    pub occurred_at: OffsetDateTime,
}

/// "Don't contact" — the operator's word that no machine letter ever goes
/// to this address again. Deliberately not a reply: the ledger stays
/// truthful about who spoke last, and the suppression is the target's own
/// standing, not a message.
#[derive(Clone, Debug)]
pub struct SuppressOutreachTarget {
    pub target_id: OutreachTargetId,
    /// `true` stamps the suppression; `false` lifts it again for a contact
    /// the operator re-opened.
    pub do_not_contact: bool,
    pub occurred_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotOutreachStateRepository: Send + Sync {
    async fn upsert_outreach_target(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertOutreachTarget,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachTargetMutation, RepositoryError>;
    async fn upsert_outreach_opportunity(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertOutreachOpportunity,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachOpportunityMutation, RepositoryError>;
    async fn record_outreach_reply(
        &self,
        workspace_id: WorkspaceId,
        command: RecordOutreachReply,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
    async fn record_outreach_written(
        &self,
        workspace_id: WorkspaceId,
        command: RecordOutreachWritten,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
    async fn suppress_outreach_target(
        &self,
        workspace_id: WorkspaceId,
        command: SuppressOutreachTarget,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct UpsertReleasePlan {
    pub release_id: Option<ReleasePlanId>,
    pub source_key: String,
    pub title: String,
    pub release_at: OffsetDateTime,
    pub listen_url: Option<String>,
    /// `None` leaves the stored tier alone on update and defaults to `track`
    /// on insert — the band's call is not silently reset by a caller that
    /// does not know about tiers yet.
    pub tier: Option<crowdrelay_domain::release_autopilot::ReleaseTier>,
    pub active: bool,
    pub assets_ready: bool,
    pub communication_enabled: bool,
    pub press_enabled: bool,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReleasePlanMutation {
    pub operation_id: uuid::Uuid,
    pub release_id: ReleasePlanId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamOpportunityKind {
    Festival,
    Showcase,
    ReviewContest,
    SupportSlot,
    Funding,
    Booking,
    Press,
    Interview,
    Sync,
}

impl TeamOpportunityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Festival => "festival",
            Self::Showcase => "showcase",
            Self::ReviewContest => "review_contest",
            Self::SupportSlot => "support_slot",
            Self::Funding => "funding",
            Self::Booking => "booking",
            Self::Press => "press",
            Self::Interview => "interview",
            Self::Sync => "sync",
        }
    }
}

#[derive(Clone, Debug)]
pub struct UpsertTeamOpportunity {
    pub opportunity_id: Option<TeamOpportunityId>,
    pub kind: TeamOpportunityKind,
    pub source: String,
    pub external_key: String,
    pub title: String,
    pub organization: String,
    pub destination_url: Option<String>,
    pub contact_email: Option<String>,
    pub verified_destination: bool,
    pub fit_basis_points: u16,
    pub reputation_basis_points: u16,
    pub confidence: Confidence,
    pub currency: String,
    pub expected_fee_minor: i64,
    pub estimated_cost_minor: i64,
    pub application_fee_minor: i64,
    pub requires_contract: bool,
    pub exclusive: bool,
    pub eligible: bool,
    pub funding_amount_minor: i64,
    pub own_contribution_minor: i64,
    pub deadline: Option<OffsetDateTime>,
    pub event_starts_at: Option<OffsetDateTime>,
    pub country_code: Option<String>,
    pub travel_band: Option<LiveTravelBand>,
    pub metadata: serde_json::Value,
    /// Operator-confirmed, `0..=10_000`. A name match against a landmark
    /// promoter or festival list is a suggestion an operator confirms here,
    /// never an automatic grant.
    pub strategic_value_basis_points: u16,
    /// When the source was actually observed — not when the row was written.
    /// `None` means there is no dated source evidence and the finding is not
    /// considered observed; staleness is measured from this, not `created_at`.
    pub source_observed_at: Option<OffsetDateTime>,
    pub expected_version: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TeamOpportunityMutation {
    pub operation_id: uuid::Uuid,
    pub opportunity_id: TeamOpportunityId,
    pub version: i64,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamOpportunityProgress {
    PackageReady,
    Submitted,
    Replied,
    Won,
    Lost,
    Dismissed,
}

#[derive(Clone, Debug)]
pub struct RecordTeamOpportunityProgress {
    pub opportunity_id: TeamOpportunityId,
    pub progress: TeamOpportunityProgress,
    pub occurred_at: OffsetDateTime,
    /// Required for terminal states (`lost`, `dismissed`): a row that closes
    /// says why, so a refusal teaches the pipeline instead of disappearing.
    /// Ignored for non-terminal progress.
    pub reason: Option<String>,
}

/// Where the promoter stands right now.
///
/// The agent never invents this. Somebody read an email and wrote down what it
/// said, and everything the negotiation does afterwards hangs off that.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromoterPosition {
    /// A fee is on the table.
    Offer { fee_minor: i64 },
    /// They have gone. The negotiation settles and nothing further is drafted.
    Withdrawn,
}

#[derive(Clone, Debug)]
pub enum DeliveryFaultSubject {
    /// The target is already known.
    Target(OutreachTargetId),
    /// The provider reported a recipient address. Resolved to the workspace's
    /// target by normalized email; an unknown address is a 404 rather than a
    /// silently dropped complaint, because a complaint about somebody who is
    /// not in the system is still a complaint about the sending domain.
    ContactEmail(String),
}

#[derive(Clone, Debug)]
pub struct RecordDeliveryFault {
    pub subject: DeliveryFaultSubject,
    pub fault: DeliveryFault,
    /// The provider's own reference, where it gave one. Webhooks retry, and a
    /// retried complaint counted twice is a halt nobody earned.
    pub provider_reference: Option<String>,
    pub occurred_at: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub struct RecordTeamOpportunityTerms {
    pub opportunity_id: TeamOpportunityId,
    pub position: PromoterPosition,
    pub currency: String,
    /// When the promoter's side of this goes cold.
    pub responds_by: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotTeamStateRepository: Send + Sync {
    /// `actor_type` names who the audit row answers for — `admin_api_key`
    /// for the control plane route, `video-watcher` for the upload sync.
    async fn upsert_release_plan(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertReleasePlan,
        actor_type: &'static str,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ReleasePlanMutation, RepositoryError>;
    async fn upsert_team_opportunity(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertTeamOpportunity,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<TeamOpportunityMutation, RepositoryError>;
    async fn record_team_opportunity_progress(
        &self,
        workspace_id: WorkspaceId,
        command: RecordTeamOpportunityProgress,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Records one bounce or spam complaint reported by the sending provider.
    ///
    /// The only way either becomes visible. A hard bounce also finishes the
    /// address, through the suppression that already exists rather than a
    /// second one invented here.
    async fn record_delivery_fault(
        &self,
        workspace_id: WorkspaceId,
        command: RecordDeliveryFault,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Records that a human submitted the Spotify editorial pitch.
    ///
    /// The only way it ever becomes true. Nothing the agent can read would tell
    /// it, and inferring it from silence is how a release goes out with no
    /// pitch and a green dashboard.
    async fn complete_editorial_pitch(
        &self,
        workspace_id: WorkspaceId,
        release_id: ReleasePlanId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Records a curator's placement claim, or the result of one public read of
    /// it.
    ///
    /// The only way a placement ever enters the system. A claim opens the row
    /// and counts toward nothing; a read folds in and may settle it.
    async fn record_playlist_placement(
        &self,
        workspace_id: WorkspaceId,
        command: RecordPlaylistPlacement,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    /// Records what the promoter has said, and opens the negotiation if this is
    /// the first thing they have said.
    ///
    /// The ladder is computed here, once, from the show's costed trip and the
    /// operator's own policy — and never recomputed, so a counter sent last
    /// week stays explainable from the row today.
    async fn record_team_opportunity_terms(
        &self,
        workspace_id: WorkspaceId,
        command: RecordTeamOpportunityTerms,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct UpsertContentSource {
    pub source_id: Option<ContentSourceId>,
    pub kind: ContentSourceKind,
    pub source_key: String,
    pub title: String,
    pub occurred_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub metadata: serde_json::Value,
    /// `None` leaves the flag alone — creates default to `active = true`,
    /// edits keep whatever the row already says. `Some` is the operator's
    /// "stop/start sharing this" control.
    pub active: Option<bool>,
    /// The catalogue format this artifact was produced in — `playthrough`,
    /// `making_of`, and so on. `None` means "leave it alone" on edit and
    /// "undeclared" on create: a source filed without a format is honest
    /// unknown, and conversion provenance reports it as unrecorded rather
    /// than a guessed bucket. Non-`None` must name a catalogue entry.
    pub format_key: Option<String>,
    pub expected_version: i64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ContentSourceMutation {
    pub operation_id: uuid::Uuid,
    pub source_id: ContentSourceId,
    pub version: i64,
    pub replayed: bool,
}
/// What the control plane renders in the real-material panel: the trusted
/// facts the engager may write about, with the metadata that carries links.
#[derive(Clone, Debug, Serialize)]
pub struct ContentSourceView {
    pub source_id: ContentSourceId,
    pub source_kind: ContentSourceKind,
    pub source_key: String,
    pub title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub metadata: serde_json::Value,
    /// The declared catalogue format, when one was filed — the operator's
    /// read on the same column provenance stamps onto conversions.
    pub format_key: Option<String>,
    pub version: i64,
    pub active: bool,
    /// What this source produced, newest first — each content-supply action
    /// taken against it with the artifact it carried and whether it actually
    /// left the building. Retiring a source must answer "what already went
    /// out", so the tombstone never hides the trail it made.
    pub sends: Vec<ContentSourceSendView>,
}

/// One artifact the machine produced from a source. `emitted_at` is the
/// moment it reached the outbox — `None` means drafted or still in flight,
/// never silently "sent".
#[derive(Clone, Debug, Serialize)]
pub struct ContentSourceSendView {
    pub action_id: uuid::Uuid,
    pub artifact: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub emitted_at: Option<OffsetDateTime>,
}
/// The two outcomes an operator may report on an approved suggestion.
/// `declined` and `expired` are not reportable — one is a decision verb on
/// the ask, the other is the sweep's verdict on a window that closed.
#[derive(Clone, Copy, Debug)]
pub enum SuggestionReportOutcome {
    Done,
    DoneDifferently,
}

impl SuggestionReportOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::DoneDifferently => "done_differently",
        }
    }
}

/// What the operator reports about one approved suggestion: the band made
/// the asked thing (`done`) or made something inspired by it
/// (`done_differently`). `reason` names what happened instead for the
/// latter — "a version of yes" without the version is a shrug the loop
/// cannot learn from. `results` carries whatever the band already measured.
#[derive(Clone, Debug)]
pub struct ReportSuggestionOutcome {
    pub suggestion_id: ContentSuggestionId,
    pub outcome: SuggestionReportOutcome,
    pub reason: Option<String>,
    pub results: serde_json::Value,
}

#[async_trait]
pub trait AutopilotContentStateRepository: Send + Sync {
    async fn upsert_content_source(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertContentSource,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ContentSourceMutation, RepositoryError>;
    /// Every content source for the panel — active and retired both, so the
    /// operator sees what the engager can draw on and what has lapsed.
    async fn list_content_sources(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ContentSourceView>, RepositoryError>;
    /// Resolves an approved suggestion with the band's report. Only
    /// `approved` rows accept a report — `raised` is still the ask's
    /// decision (approve it, or mark the decision handled-externally), and
    /// every other status is already a resolved answer.
    async fn report_suggestion_outcome(
        &self,
        workspace_id: WorkspaceId,
        command: ReportSuggestionOutcome,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;
}

#[derive(Clone, Debug)]
pub struct CreateExperimentVariant {
    pub key: String,
    pub allocation_basis_points: u16,
}
#[derive(Clone, Debug)]
pub struct CreateExperiment {
    pub slug: String,
    pub metric: ExperimentMetric,
    pub variants: Vec<CreateExperimentVariant>,
    pub start: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentMutation {
    pub operation_id: uuid::Uuid,
    pub experiment_id: ExperimentId,
    pub replayed: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct ExperimentObservation {
    pub experiment_id: ExperimentId,
    pub variant_id: ExperimentVariantId,
    pub exposures_delta: u32,
    pub conversions_delta: u32,
    pub value_minor_delta: i64,
    pub observed_at: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub struct ExperimentAssignmentVariant {
    pub slot: ExperimentAllocationSlot,
    pub key: String,
}

#[derive(Clone, Debug)]
pub struct ExperimentAssignmentSource {
    pub experiment_id: ExperimentId,
    pub version: i64,
    pub variants: Vec<ExperimentAssignmentVariant>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExperimentAssignment {
    pub experiment_id: ExperimentId,
    pub experiment_version: i64,
    pub variant_id: ExperimentVariantId,
    pub variant_key: String,
}

#[async_trait]
pub trait AutopilotExperimentStateRepository: Send + Sync {
    async fn create_experiment(
        &self,
        workspace_id: WorkspaceId,
        command: CreateExperiment,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<ExperimentMutation, RepositoryError>;

    async fn record_experiment_observation(
        &self,
        workspace_id: WorkspaceId,
        command: ExperimentObservation,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError>;

    async fn load_experiment_assignment(
        &self,
        workspace_id: WorkspaceId,
        experiment_id: ExperimentId,
    ) -> Result<ExperimentAssignmentSource, RepositoryError>;
}

pub async fn assign_experiment_variant<R: AutopilotExperimentStateRepository>(
    repository: &R,
    workspace_id: WorkspaceId,
    experiment_id: ExperimentId,
    assignment_key: &str,
) -> Result<ExperimentAssignment, RepositoryError> {
    let normalized_key = assignment_key.trim();
    if normalized_key.is_empty() || normalized_key.len() > 200 {
        return Err(RepositoryError::Unexpected);
    }

    let source = repository
        .load_experiment_assignment(workspace_id, experiment_id)
        .await?;
    let slots = source
        .variants
        .iter()
        .map(|variant| variant.slot)
        .collect::<Vec<_>>();
    let selected = assign_variant(experiment_id, normalized_key.as_bytes(), &slots)
        .ok_or(RepositoryError::Conflict)?;
    let variant = source
        .variants
        .into_iter()
        .find(|variant| variant.slot.variant_id == selected)
        .ok_or(RepositoryError::Unexpected)?;

    Ok(ExperimentAssignment {
        experiment_id: source.experiment_id,
        experiment_version: source.version,
        variant_id: selected,
        variant_key: variant.key,
    })
}

#[async_trait]
pub trait AutopilotMarketStateRepository: Send + Sync {
    async fn upsert_promotion_budget_guardrail(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertPromotionBudgetGuardrail,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<PromotionBudgetGuardrailMutation, RepositoryError>;

    async fn upsert_promotion_campaign_state(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertPromotionCampaignState,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<PromotionCampaignStateMutation, RepositoryError>;

    async fn upsert_city_market_signal(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertCityMarketSignal,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<CityMarketSignalMutation, RepositoryError>;
}
