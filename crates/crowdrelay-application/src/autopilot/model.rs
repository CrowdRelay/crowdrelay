//! Stable application-boundary types for ViryaOS Autopilot.

use crowdrelay_brain::{AgentTier, GrowthIntelligencePolicy};
use crowdrelay_domain::{
    ArcId, AutopilotActionId, BeaconId, BookingAgentId, BookingTargetId, CityId, ContentSourceId,
    ContentSuggestionId, EventId, ExperimentId, ExperimentVariantId, FanId, GrowthMetricSeriesId,
    MerchProductId, MerchVariantId, OutreachOpportunityId, OutreachTargetId, PlayId,
    PromotionCampaignId, ReleasePlanId, TeamOpportunityId, TicketTypeId, WorkspaceId,
    action_class::ActionClass,
    audience_lifecycle::FanLifecyclePolicy,
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    beacons::{BeaconCampaignPolicy, BeaconOutreachPhase},
    booking::{BookingOpportunityPolicy, BookingOutreachPhase, BookingVenueEvidence},
    booking_agent::{AgentDrawEvidence, BookingAgentPolicy},
    booking_window::ProposedWindow,
    campaign_lifecycle::{EventCampaignPhase, EventCampaignPolicy},
    content_engine::ContentStrategyPolicy,
    content_supply::{ContentArtifactKind, ContentSupplyPolicy},
    experimentation::ExperimentPolicy,
    free_reach::{WaveAnchor, WaveExpiry},
    funding::FundingPolicy,
    growth_debt::{GrowthDebtKind, GrowthDebtPolicy, GrowthDebtSubject},
    growth_metrics::{GrowthMetricPolicy, GrowthSignal, MetricDirection, MetricPlatform},
    learning::{OutcomeRecord, Standing},
    live_opportunities::{LiveOpportunityKind, LiveOpportunityPolicy, LiveOpportunitySnapshot},
    merch_bundle::MerchBundlePolicy,
    merchandising::{MerchPricePolicy, MerchReorderPolicy},
    negotiation::{TermsRefusal, TermsSnapshot, TermsState},
    outreach::{OutreachPhase, OutreachPolicy, OutreachTargetKind},
    play_measurement::PlayClaim,
    playlist_placement::{PlacementObservation, PlacementSnapshot, PlacementState},
    plays::{PlayAnchorKind, PlayKind, PlayPolicy, PlayStepKind, PlayStepState, StepSkipReason},
    pricing::TicketYieldPolicy,
    promotion::PromotionBudgetPolicy,
    release_autopilot::{ReleaseAutopilotPolicy, ReleaseMilestone},
    representation::RepresentationPolicy,
    show_growth::{ShowGrowthLever, ShowGrowthPolicy},
    show_operations::{ShowOperationsPolicy, ShowTaskKind},
    target_discovery::OutreachSupplyPolicy,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotContext {
    TicketYield,
    FanLifecycle,
    CampaignLifecycle,
    Merchandising,
    MerchPricing,
    MerchBundle,
    BookingOpportunity,
    Outreach,
    ContentSupply,
    PromotionBudget,
    Experimentation,
    ShowOperations,
    Release,
    LiveOpportunity,
    Funding,
    Beacon,
    ShowGrowth,
    GrowthMetrics,
    GrowthDebt,
    OutreachSupply,
    /// The deterministic brain. Decides what intelligence to gather, when,
    /// and what to do with it. Dispatches LLM workers via `RequestAgentRun`
    /// actions. Never follows an LLM blindly — it applies deterministic rules.
    GrowthIntelligence,
    /// The only context that remembers. Every other one answers a question per
    /// cycle and forgets; a play carries a campaign across cycles, restarts and
    /// deploys, which is what lets the agent do step two of anything.
    Plays,
    /// The suggestion engine's route to the band. Strategy proposes; the
    /// queue surfaces each raised suggestion with its evidence and the band
    /// commits — creative work stays human in every posture.
    ContentStrategy,
    /// Representation approaches: the band picks a consented agent or label
    /// and the approach queues for approval. Deterministic gates own whether
    /// one may go out — consent, activity, verification, monthly allowance.
    Representation,
    /// Booking-agent approaches (§4h-10): the band picks a screened agent and
    /// asks for representation — a season-scarce application that refuses to
    /// exist without real draw evidence behind it.
    BookingAgent,
}

impl AutopilotContext {
    /// Every context, in policy-store order.
    ///
    /// Storage parsing is derived from this list rather than restating the
    /// names: a context the policy table can hold but a reader cannot parse
    /// fails the whole overview read, not just its own row.
    pub const ALL: [Self; 25] = [
        Self::TicketYield,
        Self::FanLifecycle,
        Self::CampaignLifecycle,
        Self::Merchandising,
        Self::MerchPricing,
        Self::MerchBundle,
        Self::BookingOpportunity,
        Self::Outreach,
        Self::ContentSupply,
        Self::PromotionBudget,
        Self::Experimentation,
        Self::ShowOperations,
        Self::Release,
        Self::LiveOpportunity,
        Self::Funding,
        Self::Beacon,
        Self::ShowGrowth,
        Self::GrowthMetrics,
        Self::GrowthDebt,
        Self::OutreachSupply,
        Self::GrowthIntelligence,
        Self::Plays,
        Self::ContentStrategy,
        Self::Representation,
        Self::BookingAgent,
    ];

    /// Parse the stored representation written by [`Self::as_str`].
    #[must_use]
    pub fn from_storage(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|context| context.as_str() == value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TicketYield => "ticket_yield",
            Self::FanLifecycle => "fan_lifecycle",
            Self::CampaignLifecycle => "campaign_lifecycle",
            Self::Merchandising => "merchandising",
            Self::MerchPricing => "merch_pricing",
            Self::MerchBundle => "merch_bundle",
            Self::BookingOpportunity => "booking_opportunity",
            Self::Outreach => "outreach",
            Self::ContentSupply => "content_supply",
            Self::PromotionBudget => "promotion_budget",
            Self::Experimentation => "experimentation",
            Self::ShowOperations => "show_operations",
            Self::Release => "release",
            Self::LiveOpportunity => "live_opportunity",
            Self::Funding => "funding",
            Self::Beacon => "beacon",
            Self::ShowGrowth => "show_growth",
            Self::GrowthMetrics => "growth_metrics",
            Self::GrowthDebt => "growth_debt",
            Self::OutreachSupply => "outreach_supply",
            Self::GrowthIntelligence => "growth_intelligence",
            Self::Plays => "plays",
            Self::ContentStrategy => "content_strategy",
            Self::Representation => "representation",
            Self::BookingAgent => "booking_agent",
        }
    }
}

/// Typed bounded-context configuration loaded from the policy store.
#[derive(Clone, Debug, PartialEq)]
pub enum AutopilotPolicyConfig {
    TicketYield(TicketYieldPolicy),
    FanLifecycle(FanLifecyclePolicy),
    CampaignLifecycle(EventCampaignPolicy),
    Merchandising(MerchReorderPolicy),
    MerchPricing(MerchPricePolicy),
    MerchBundle(MerchBundlePolicy),
    BookingOpportunity(BookingOpportunityPolicy),
    Outreach(OutreachPolicy),
    ContentSupply(ContentSupplyPolicy),
    PromotionBudget(PromotionBudgetPolicy),
    Experimentation(ExperimentPolicy),
    ShowOperations(ShowOperationsPolicy),
    Release(ReleaseAutopilotPolicy),
    LiveOpportunity(LiveOpportunityPolicy),
    Funding(FundingPolicy),
    Beacon(BeaconCampaignPolicy),
    ShowGrowth(ShowGrowthPolicy),
    GrowthMetrics(GrowthMetricPolicy),
    GrowthDebt(GrowthDebtPolicy),
    OutreachSupply(OutreachSupplyPolicy),
    GrowthIntelligence(GrowthIntelligencePolicy),
    Plays(PlayPolicy),
    ContentStrategy(ContentStrategyPolicy),
    Representation(RepresentationPolicy),
    BookingAgent(BookingAgentPolicy),
}

impl AutopilotPolicyConfig {
    /// Parses one context's operator config from its stored JSON.
    ///
    /// The single source of truth for "what config keys does this context
    /// accept": the policy reader, the write path and the API validator all
    /// call this, so a key cannot be accepted on write and silently dropped
    /// on read. An empty object means "reset to defaults" — every knob is
    /// optional and every type carries its own defaults.
    pub fn parse_for(
        context: AutopilotContext,
        raw: serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        match context {
            AutopilotContext::TicketYield => {
                Self::parse_into(raw, Self::TicketYield, TicketYieldPolicy::default())
            }
            AutopilotContext::FanLifecycle => {
                Self::parse_into(raw, Self::FanLifecycle, FanLifecyclePolicy::default())
            }
            AutopilotContext::CampaignLifecycle => {
                Self::parse_into(raw, Self::CampaignLifecycle, EventCampaignPolicy::default())
            }
            AutopilotContext::Merchandising => {
                Self::parse_into(raw, Self::Merchandising, MerchReorderPolicy::default())
            }
            AutopilotContext::MerchPricing => {
                Self::parse_into(raw, Self::MerchPricing, MerchPricePolicy::default())
            }
            AutopilotContext::MerchBundle => {
                Self::parse_into(raw, Self::MerchBundle, MerchBundlePolicy::default())
            }
            AutopilotContext::BookingOpportunity => Self::parse_into(
                raw,
                Self::BookingOpportunity,
                BookingOpportunityPolicy::default(),
            ),
            AutopilotContext::Outreach => {
                Self::parse_into(raw, Self::Outreach, OutreachPolicy::default())
            }
            AutopilotContext::ContentSupply => {
                Self::parse_into(raw, Self::ContentSupply, ContentSupplyPolicy::default())
            }
            AutopilotContext::PromotionBudget => {
                Self::parse_into(raw, Self::PromotionBudget, PromotionBudgetPolicy::default())
            }
            AutopilotContext::Experimentation => {
                Self::parse_into(raw, Self::Experimentation, ExperimentPolicy::default())
            }
            AutopilotContext::ShowOperations => {
                let parsed =
                    Self::parse_into(raw, Self::ShowOperations, ShowOperationsPolicy::default())?;
                if let Self::ShowOperations(policy) = &parsed {
                    policy.validate().map_err(serde::de::Error::custom)?;
                }
                Ok(parsed)
            }
            AutopilotContext::Release => {
                Self::parse_into(raw, Self::Release, ReleaseAutopilotPolicy::default())
            }
            AutopilotContext::LiveOpportunity => {
                Self::parse_into(raw, Self::LiveOpportunity, LiveOpportunityPolicy::default())
            }
            AutopilotContext::Funding => {
                Self::parse_into(raw, Self::Funding, FundingPolicy::default())
            }
            AutopilotContext::Beacon => {
                Self::parse_into(raw, Self::Beacon, BeaconCampaignPolicy::default())
            }
            AutopilotContext::ShowGrowth => {
                Self::parse_into(raw, Self::ShowGrowth, ShowGrowthPolicy::default())
            }
            AutopilotContext::GrowthMetrics => {
                Self::parse_into(raw, Self::GrowthMetrics, GrowthMetricPolicy::default())
            }
            AutopilotContext::GrowthDebt => {
                Self::parse_into(raw, Self::GrowthDebt, GrowthDebtPolicy::default())
            }
            AutopilotContext::OutreachSupply => {
                Self::parse_into(raw, Self::OutreachSupply, OutreachSupplyPolicy::default())
            }
            AutopilotContext::GrowthIntelligence => Self::parse_into(
                raw,
                Self::GrowthIntelligence,
                GrowthIntelligencePolicy::default(),
            ),
            AutopilotContext::Plays => Self::parse_into(raw, Self::Plays, PlayPolicy::default()),
            AutopilotContext::ContentStrategy => {
                Self::parse_into(raw, Self::ContentStrategy, ContentStrategyPolicy::default())
            }
            AutopilotContext::Representation => {
                Self::parse_into(raw, Self::Representation, RepresentationPolicy::default())
            }
            AutopilotContext::BookingAgent => {
                Self::parse_into(raw, Self::BookingAgent, BookingAgentPolicy::default())
            }
        }
    }

    fn parse_into<T>(
        raw: serde_json::Value,
        wrap: fn(T) -> Self,
        default: T,
    ) -> Result<Self, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        // An empty object is the reset-to-defaults spelling, matching how a
        // provisioned workspace reads before anybody has tuned anything.
        if raw.as_object().is_some_and(serde_json::Map::is_empty) {
            return Ok(wrap(default));
        }
        serde_json::from_value::<T>(raw).map(wrap)
    }
}

/// Persisted authority configuration for one bounded context.
#[derive(Clone, Debug, PartialEq)]
pub struct AutopilotPolicy {
    pub context: AutopilotContext,
    pub enabled: bool,
    pub autonomy_level: AutonomyLevel,
    pub minimum_confidence: Confidence,
    pub max_actions_24h: u32,
    pub config: AutopilotPolicyConfig,
    /// Monotonic configuration version used to make decision evidence immutable.
    pub version: i64,
    pub guarded_until: Option<OffsetDateTime>,
    pub guardrail_reason: Option<String>,
}

/// Generic subject reference used only at the application/action boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionSubject {
    TicketType(TicketTypeId),
    Fan(FanId),
    MerchVariant(MerchVariantId),
    MerchProduct(MerchProductId),
    City(CityId),
    Event(EventId),
    OutreachOpportunity(OutreachOpportunityId),
    ContentSource(ContentSourceId),
    Experiment(ExperimentId),
    PromotionCampaign(PromotionCampaignId),
    ReleasePlan(ReleasePlanId),
    TeamOpportunity(TeamOpportunityId),
    Beacon(BeaconId),
    GrowthMetricSeries(GrowthMetricSeriesId),
    BookingTarget(BookingTargetId),
    /// A screened booking agent — `viryaos_booking_agents.id`. Distinct from
    /// `BookingTarget`: the agent is the booking graph's third entity, the
    /// one the band applies to rather than pitches a night at.
    BookingAgent(BookingAgentId),
    OutreachTarget(OutreachTargetId),
    /// P0-3: A community target discovered by the agent (e.g. a subreddit
    /// from `agent_outreach_targets`). Distinct from `OutreachTarget`
    /// which is an operator-managed contact in `viryaos_outreach_targets`.
    /// The UUID is `agent_outreach_targets.id`.
    TargetCommunity(uuid::Uuid),
    /// Supply is a property of the whole workspace rather than of any one row,
    /// so the sweep that replenishes it has the workspace as its subject.
    Workspace(WorkspaceId),
    /// One raised content suggestion — the queue entry that asks the band to
    /// commit to a beat. The UUID is `viryaos_content_suggestions.id`.
    ContentSuggestion(ContentSuggestionId),
    /// One proposed arc — the queue entry that asks the band to commit to a
    /// season's shape. The UUID is `viryaos_arcs.id`. The arc, not any beat
    /// inside it, is the approval unit.
    ContentArc(ArcId),
}

impl From<GrowthDebtSubject> for ActionSubject {
    fn from(subject: GrowthDebtSubject) -> Self {
        match subject {
            GrowthDebtSubject::BookingTarget(id) => Self::BookingTarget(id),
            GrowthDebtSubject::OutreachTarget(id) => Self::OutreachTarget(id),
            GrowthDebtSubject::Beacon(id) => Self::Beacon(id),
            GrowthDebtSubject::Event(id) => Self::Event(id),
            GrowthDebtSubject::ReleasePlan(id) => Self::ReleasePlan(id),
            GrowthDebtSubject::Workspace(id) => Self::Workspace(id),
        }
    }
}

impl ActionSubject {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::TicketType(_) => "ticket_type",
            Self::Fan(_) => "fan",
            Self::MerchVariant(_) => "merch_variant",
            Self::MerchProduct(_) => "merch_product",
            Self::City(_) => "city",
            Self::Event(_) => "event",
            Self::OutreachOpportunity(_) => "outreach_opportunity",
            Self::ContentSource(_) => "content_source",
            Self::Experiment(_) => "experiment",
            Self::PromotionCampaign(_) => "promotion_campaign",
            Self::ReleasePlan(_) => "release_plan",
            Self::TeamOpportunity(_) => "team_opportunity",
            Self::Beacon(_) => "beacon",
            Self::GrowthMetricSeries(_) => "growth_metric_series",
            Self::BookingTarget(_) => "booking_target",
            Self::BookingAgent(_) => "booking_agent",
            Self::OutreachTarget(_) => "outreach_target",
            Self::TargetCommunity(_) => "target_community",
            Self::Workspace(_) => "workspace",
            Self::ContentSuggestion(_) => "content_suggestion",
            Self::ContentArc(_) => "content_arc",
        }
    }

    /// True when the subject *is* the person being contacted.
    ///
    /// The envelope's cooldown exists so no one hears from the agent twice in a
    /// week. That only means anything when the subject is a contact. An event
    /// or a release is a *topic*: a show legitimately needs a listing sweep, an
    /// ambassador push and a last-mile nudge over the weeks before it, each
    /// reaching different people, and a cooldown keyed on the event would allow
    /// exactly one of them per week and silently starve the rest.
    ///
    /// Per-recipient frequency for those campaigns is the audience filter's job,
    /// not this one — the agent cannot enforce at the action level something it
    /// only resolves at delivery.
    #[must_use]
    pub const fn is_contactable_person(self) -> bool {
        matches!(
            self,
            Self::Fan(_)
                | Self::BookingTarget(_)
                | Self::BookingAgent(_)
                | Self::OutreachTarget(_)
                | Self::Beacon(_)
        )
    }

    #[must_use]
    pub fn uuid(self) -> uuid::Uuid {
        match self {
            Self::TicketType(id) => id.into_uuid(),
            Self::Fan(id) => id.into_uuid(),
            Self::MerchVariant(id) => id.into_uuid(),
            Self::MerchProduct(id) => id.into_uuid(),
            Self::City(id) => id.into_uuid(),
            Self::Event(id) => id.into_uuid(),
            Self::OutreachOpportunity(id) => id.into_uuid(),
            Self::ContentSource(id) => id.into_uuid(),
            Self::Experiment(id) => id.into_uuid(),
            Self::PromotionCampaign(id) => id.into_uuid(),
            Self::ReleasePlan(id) => id.into_uuid(),
            Self::TeamOpportunity(id) => id.into_uuid(),
            Self::Beacon(id) => id.into_uuid(),
            Self::GrowthMetricSeries(id) => id.into_uuid(),
            Self::BookingTarget(id) => id.into_uuid(),
            Self::BookingAgent(id) => id.into_uuid(),
            Self::OutreachTarget(id) => id.into_uuid(),
            Self::TargetCommunity(id) => id,
            Self::Workspace(id) => id.into_uuid(),
            Self::ContentSuggestion(id) => id.into_uuid(),
            Self::ContentArc(id) => id.into_uuid(),
        }
    }
}

/// Typed executable intents. Infrastructure serializes these only at the
/// durable action boundary; decision services never manipulate JSON blobs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExperimentAllocation {
    pub variant_id: ExperimentVariantId,
    pub allocation_basis_points: u16,
}

/// One promoter on a gig outreach, pinned to the row the proposal read.
///
/// The version is the whole of the optimistic concurrency: a target edited
/// between the approval and the send fails the lock rather than being written
/// to under stale terms. The address is not here and never is — it is read
/// inside the sending transaction, after the gates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GigOutreachRecipient {
    pub target_id: BookingTargetId,
    pub target_version: i64,
    pub target_name: String,
}

/// Which letter a `RequestGigOutreach` sends.
///
/// Same action, same capability, same all-or-none recipient reservation — the
/// letter is what differs. `Proposal` is the band asking a room for a night;
/// `SupportSlotAsk` is the headliner's own workspace asking its promoter to
/// confirm a named labelmate for a slot that is already on the bill. The two
/// render from different template keys, which is why the kind is on the
/// payload rather than decided at dispatch.
///
/// `Default` is `Proposal` because every action queued before this field
/// existed is one — the serde default decodes those rows unchanged.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GigLetterKind {
    /// The proposal letter: "we want to book a night here".
    #[default]
    Proposal,
    /// The support-slot ask: "{support_act} can take the open slot on the
    /// night we already hold". Carries the facts the template needs that the
    /// proposal letter never has — whose name is being put forward and which
    /// show the slot belongs to.
    SupportSlotAsk {
        /// The roster act being proposed, by the name the promoter reads.
        support_act: String,
        /// The show the slot is on — the letter's subject, and the row a
        /// receipt traces back to.
        event_id: EventId,
        /// The date as the letter states it, rendered at approval time — the
        /// show may move later, and the letter says what was true when it was
        /// asked.
        show_date: String,
    },
}

impl GigLetterKind {
    /// The template an executor renders this letter from.
    #[must_use]
    pub const fn template_key(&self) -> &'static str {
        match self {
            Self::Proposal => "gig.proposal.v1",
            Self::SupportSlotAsk { .. } => "support.slot.ask.v1",
        }
    }

    /// Whether this is the proposal — used to keep old payloads byte-identical:
    /// a proposal letter serializes without a `letter` key at all.
    #[must_use]
    pub const fn is_proposal(&self) -> bool {
        matches!(self, Self::Proposal)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutopilotActionPayload {
    ChangeTicketPrice {
        ticket_type_id: TicketTypeId,
        from_minor: i64,
        to_minor: i64,
    },
    ChangeTicketCapacity {
        ticket_type_id: TicketTypeId,
        from_capacity: u32,
        to_capacity: u32,
        guardrail_version: i64,
    },
    RequestFanLifecycleMessage {
        fan_id: FanId,
        template_key: String,
    },
    RequestMerchReorder {
        variant_id: MerchVariantId,
        quantity: u32,
    },
    ChangeMerchPrice {
        product_id: MerchProductId,
        from_minor: i64,
        to_minor: i64,
        economics_version: i64,
    },
    RequestBookingOutreach {
        city_id: CityId,
        target_id: BookingTargetId,
        target_version: i64,
        target_name: String,
        score: u16,
        phase: BookingOutreachPhase,
        /// §12-6: the derived window — `None` means no basis, and the action
        /// still carries the room; "sometime in spring" is what the band
        /// would have written anyway.
        #[serde(default)]
        proposed_window: Option<ProposedWindow>,
        /// §12-6: "write to A, B and C" is ONE action — the next-ranked
        /// same-city targets copied on the same letter, each
        /// (target_id, expected_version). The governor reserves all or none.
        #[serde(default)]
        additional_recipients: Vec<(BookingTargetId, i64)>,
        /// The evidence row the operator approved — carried verbatim so the
        /// send renders its `first_line_fact` from the same facts the
        /// approval showed. `None` for an unlinked or evidenceless room.
        #[serde(default)]
        venue_evidence: Option<BookingVenueEvidence>,
    },
    RequestAudienceCampaign {
        event_id: EventId,
        phase: EventCampaignPhase,
        template_key: String,
        /// How many fans this reaches, when the decision snapshot knows (O.5).
        ///
        /// `None` is honest rather than convenient: the announcement's audience
        /// is every consented fan in the event's city and is counted when the
        /// campaign is built, so the approval says so instead of printing a
        /// zero that would read as "nobody".
        #[serde(default)]
        audience_size: Option<u32>,
        /// Who those people are, in a sentence — "fans who said they are coming
        /// and have not bought a ticket". An operator approving a send to
        /// strangers' inboxes should not have to decode a segment slug to learn
        /// who is in it.
        #[serde(default)]
        audience_basis: String,
        /// The words the campaign sends, composed in-repo at raise time (O.3):
        /// the event's own facts in the tenant's voice. The mailer sends this
        /// verbatim — the approval shows exactly what a fan reads, and
        /// `draft_revision` can offer `subject`/`body` for editing because the
        /// text lives in the payload.
        ///
        /// `default` for the rows queued before it existed: those carry an
        /// empty draft and execution refuses them rather than inventing the
        /// missing words.
        #[serde(default)]
        draft: crowdrelay_domain::campaign_lifecycle::EventCampaignCopy,
    },
    RequestMerchBundle {
        product_a: MerchProductId,
        product_b: MerchProductId,
        bundle_price_minor: i64,
        affinity_basis_points: u16,
    },
    RequestOutreach {
        opportunity_id: OutreachOpportunityId,
        target_id: OutreachTargetId,
        target_version: i64,
        target_name: String,
        phase: OutreachPhase,
        template_key: String,
        /// The wave this pitch belongs to, when it belongs to one.
        ///
        /// Membership lives here rather than in a column on the actions table:
        /// a wave is one context's concern and that is the hottest table in
        /// the system. Absent means an ordinary standing pitch, approved on its
        /// own like every other.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wave_id: Option<uuid::Uuid>,
    },
    /// Ask the platform to introduce the band to a representation contact —
    /// a booking agent or a label the band wants carrying its career. The
    /// published listing is the pitch: the emitted event carries its
    /// redacted claims and share link, and dispatch refuses if the listing
    /// has been unlisted since the approach was requested.
    ///
    /// The agent's address never reaches the band, so the send is brokered
    /// and our reputation rides on it — which is why the consent, evidence
    /// and allowance gates in `crowdrelay_domain::representation` run again
    /// in the execution arm rather than only at request time.
    RequestRepresentationApproach {
        target_id: OutreachTargetId,
        target_version: i64,
        target_name: String,
        /// One line of the band's own words under the listing — "we met
        /// after the Wrocław show". Capped at the API boundary; absent means
        /// the listing speaks alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// The draw numbers the request measured — an agent pitch cites
        /// them, so what the approval saw is what the letter claims.
        /// `None` on a label: the listing is its pitch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draw_evidence: Option<crowdrelay_domain::representation::DrawEvidence>,
    },
    /// Ask the platform to broker the band's application to a booking agent
    /// (§4h-10): representation for a season, not a slot on one date.
    ///
    /// The proof is the pitch — `evidence` is the draw snapshot the approval
    /// screen showed, carried verbatim so the send argues from the same
    /// numbers. Dispatch re-runs the whole gate — standing, the season door,
    /// and a fresh evidence floor — so an approval that went stale cannot
    /// send, and a decline since then closes the door under it.
    ///
    /// The agent's address never reaches the band, so the send is brokered
    /// and our reputation rides on it — which is why a thin approach is
    /// refused outright rather than sent weaker.
    RequestBookingAgentApproach {
        agent_id: BookingAgentId,
        agent_version: i64,
        agent_name: String,
        /// The agency the agent works for — half of who they are. Absent is
        /// honest: an independent is not an agency with no name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agency: Option<String>,
        /// One line of the band's own words over the numbers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// The draw readings the approval was made on — the pitch's content,
        /// not an attachment to it.
        evidence: AgentDrawEvidence,
    },
    /// Write to everybody who books one room about one night (§12-6, 4G.4).
    ///
    /// # Why this is one action and not one per promoter
    ///
    /// A gig proposal names two or three people who book the same city. Queued
    /// as separate actions they reserve contact windows separately, so the
    /// governor can admit the first and refuse the second — and the band has
    /// then written to one of three promoters about a night, which reads as
    /// either a snub or a shambles depending on who compares notes. One action
    /// with a recipient set reserves all of them inside one transaction:
    /// everybody hears, or nobody does and the band is told why.
    ///
    /// The band approved the proposal, so this carries the reasons it was
    /// approved on. `opening_line` is rendered by `domain::gig_plan` from the
    /// same reason the console displayed, because a letter that argues
    /// something the proposal did not say is a letter the band cannot defend.
    RequestGigOutreach {
        city_id: CityId,
        /// The room the night is proposed at. Named in the letter, so a
        /// promoter who books two rooms knows which one is meant.
        venue: String,
        /// Everybody who books here, in the order the proposal ranked them.
        recipients: Vec<GigOutreachRecipient>,
        /// The proposal's strongest reason as one sentence, from
        /// `GigPlan::opening_line`.
        opening_line: String,
        /// Every reason the proposal was made of, as rendered sentences, so
        /// the draft can use more than the first without re-deriving any of
        /// them.
        reasons: Vec<String>,
        /// Which letter this is. Absent on every action queued before the
        /// support-slot ask existed — those are all proposals, so the default
        /// decodes them unchanged, and a proposal serializes without the key
        /// rather than carrying a field that says nothing.
        #[serde(default, skip_serializing_if = "GigLetterKind::is_proposal")]
        letter: GigLetterKind,
        /// The finished letter: the subject and every sentence the promoter
        /// will read (O.1).
        ///
        /// It used to be composed in the executor's JavaScript after the
        /// approval, so the band approved an opening line and a promoter
        /// received five paragraphs nobody at the band had seen. Carrying it
        /// here is what lets the console show the real thing, lets
        /// `draft_revision` offer `subject` and `body` for editing, and lets
        /// the identical-draft refusal compare drafts at all.
        ///
        /// `default` for the rows queued before it existed: those carry an
        /// empty draft and the executor refuses them rather than inventing the
        /// missing words.
        #[serde(default)]
        draft: crowdrelay_domain::gig_letter::GigLetter,
    },
    /// Read a public playlist and report whether the track is in it.
    ///
    /// Contacts nobody and changes nothing outside the workspace, so it is
    /// `first_party_reversible` — but it does need an executor with a Spotify
    /// credential, and until one advertises `playlist.verify` these park rather
    /// than pretending a claim was checked.
    VerifyPlaylistPlacement {
        opportunity_id: OutreachOpportunityId,
        playlist_external_id: String,
        track_external_id: String,
        /// Which of the three reads this is. Named so a late worker cannot
        /// satisfy two checkpoints with one read.
        checkpoint: u8,
    },
    RequestBeaconDiscovery {
        event_id: EventId,
        target_count: u16,
    },
    /// Ask a verified scene node to run an invite batch for one of their own
    /// city's shows. The beacon hands their community invite codes; the codes
    /// are ours, so every signup that comes back is attributed and consented
    /// by construction. Third-party: it is a request to a partner, not a
    /// message to our audience.
    RequestBeaconInviteBatch {
        beacon_id: BeaconId,
        beacon_version: i64,
        event_id: EventId,
        requested_count: u16,
    },
    /// Ask an adapter to sweep published sources for submission routes and post
    /// the candidates back. Reads public data, contacts nobody, and buys
    /// nothing; the screening that decides what is admissible stays here.
    RequestOutreachDiscovery {
        requested_candidates: u16,
    },
    /// Ask an adapter to sweep published venue/promoter routes and post
    /// booking candidates back. Reads public data, contacts nobody, buys
    /// nothing; screening and the third-party ceiling keep every judgement
    /// about who may be approached exactly where it already lives.
    RequestBookingTargetDiscovery {
        requested_count: u16,
    },
    /// Ask somebody the band already works with whether they also want the
    /// dates first (P.1).
    ///
    /// Not a campaign and not a newsletter signup. One letter, to one person
    /// the band has a relationship with, carrying a reason that is about them —
    /// a date in their city, a night you shared, a record just out. The rules
    /// that decide whether it may go at all live in
    /// `crowdrelay_domain::latarnik_invite`, and they are almost entirely
    /// refusals.
    ///
    /// `draft` carries the finished letter for the same reason the gig letter
    /// does (O.1): the operator approves the sentences a promoter will read,
    /// not a template key that becomes sentences later.
    RequestLatarnikInvite {
        beacon_id: BeaconId,
        /// Pinned at approval time. A beacon edited between the read and the
        /// send is a different person's record, and the send refuses.
        beacon_version: i64,
        recipient_email: String,
        recipient_name: String,
        /// Why this person, in the words the letter opens with. Carried so the
        /// receipt and the ledger record what was claimed, not only that
        /// something was sent.
        reason: String,
        draft: crowdrelay_domain::latarnik_invite::Invite,
    },
    RequestBeaconOutreach {
        beacon_id: BeaconId,
        event_id: EventId,
        beacon_version: i64,
        phase: BeaconOutreachPhase,
        template_key: String,
    },
    RequestShowGrowth {
        event_id: EventId,
        lever: ShowGrowthLever,
        template_key: String,
        /// Scheduled reach time for the lever's campaign, decided at evaluation
        /// time so the cadence is part of the durable action evidence. `None`
        /// (and older payloads without the field) send when execution runs.
        #[serde(default)]
        send_at: Option<OffsetDateTime>,
    },
    RequestContentArtifact {
        source_id: ContentSourceId,
        source_version: i64,
        artifact: ContentArtifactKind,
        template_key: String,
    },
    AdjustExperiment {
        experiment_id: ExperimentId,
        expected_version: i64,
        winner_variant_id: ExperimentVariantId,
        allocations: Vec<ExperimentAllocation>,
        complete: bool,
    },
    CompleteShowTask {
        event_id: EventId,
        task: ShowTaskKind,
    },
    EscalateShowTask {
        event_id: EventId,
        task: ShowTaskKind,
    },
    RequestPromotionBudgetChange {
        campaign_id: PromotionCampaignId,
        from_minor: i64,
        to_minor: i64,
        roas_basis_points: u32,
    },
    ExecuteReleaseMilestone {
        release_id: ReleasePlanId,
        title: String,
        release_at: time::OffsetDateTime,
        milestone: ReleaseMilestone,
    },
    /// Chase an unsubmitted Spotify editorial pitch. A nudge inside the
    /// workspace, never a claim that anything was submitted.
    EscalateEditorialPitch {
        release_id: ReleasePlanId,
        title: String,
        due_at: time::OffsetDateTime,
    },
    ApplyLiveOpportunity {
        opportunity_id: TeamOpportunityId,
        opportunity_kind: LiveOpportunityKind,
        score: u16,
    },
    /// Ask the promoter for a different fee. Drafted by the agent, sent by a
    /// human at the current posture.
    CounterLiveOpportunityTerms {
        opportunity_id: TeamOpportunityId,
        ask_minor: i64,
        currency: String,
        /// Which ask this is. Carried so the message can say "as discussed"
        /// rather than opening the conversation again.
        round: u8,
    },
    /// Take the fee on the table. Refused outright by the domain when the show
    /// requires a contract, is exclusive, has no free date, could not be costed
    /// or would push the year past its stretch — at every autonomy level.
    AcceptLiveOpportunityTerms {
        opportunity_id: TeamOpportunityId,
        fee_minor: i64,
        currency: String,
    },
    PrepareFundingPackage {
        opportunity_id: TeamOpportunityId,
    },
    SubmitFundingApplication {
        opportunity_id: TeamOpportunityId,
    },
    /// Surfaces a detected movement in an external metric as durable work. The
    /// payload carries the evidence and the class of response, never a
    /// provider call: what a platform can actually do is not the domain's to
    /// assume, so execution stays with the operator or an executor that owns
    /// that capability.
    RaiseGrowthOpportunity {
        series_id: GrowthMetricSeriesId,
        platform: MetricPlatform,
        metric_key: String,
        signal: GrowthSignal,
        recommended_action: String,
        /// Measured deviation from the series' own baseline, in basis points.
        deviation_basis_points: u32,
        priority: u16,
        template_key: String,
    },
    /// Uncomfortable advice (§4g): a community the engager keeps posting
    /// to that engages yet has produced zero fans. The finding is the work
    /// — evidence and the alternative travel in the payload, never "stop X"
    /// without "do Y instead". Approving parks the place (`not_a_fit`) so
    /// the engager stops spending there; cancelling is the band's recorded
    /// disagreement, and the same subject is not re-raised for a month.
    RaiseDeclineAdvisory {
        /// The outreach target whose community the advisory is about —
        /// also the action's subject, so the inflight index dedupes it.
        target_id: uuid::Uuid,
        /// The discovery place an approval parks. NULL when the target
        /// never matched a place row — the advisory still carries its
        /// evidence, there is just nothing to switch off.
        place_id: Option<uuid::Uuid>,
        subreddit: String,
        /// Posts with metrics inside the window, and their average score —
        /// the "engages" half of the claim. Tenths of a point: 12.3 is
        /// stored as 123, because the payload derives `Eq` and `f64` has none.
        posts_considered: u32,
        avg_score_tenths: i64,
        /// Measurement window for both engagement and conversions.
        window_days: u32,
        /// The paired alternative — a community that converted, or the
        /// strongest other room when nothing converts yet.
        alternative_label: String,
        alternative_detail: String,
    },
    /// Give a consented fan a referral code.
    ///
    /// The only growth mechanism that scales with the audience rather than with
    /// the band's effort, and it has to exist before anything invites anybody.
    IssueReferralCode {
        fan_id: FanId,
    },
    /// The content engine's ask: commit to this beat. The suggestion row
    /// already carries the concept, the reason, the evidence and the
    /// distribution promise; this action is the surface the band answers on.
    ///
    /// Approving marks the suggestion `approved` — the work is committed and
    /// stays open until the band reports done, declined or done-differently,
    /// or until the beat's day passes unreported (the sweep resolves it
    /// `expired` — the window closed; whether it happened is unmeasured).
    /// Cancelling resolves it `declined`: "not for us" is a first-class taste
    /// signal the engine learns from, not a dismissal.
    RaiseContentSuggestion {
        suggestion_id: ContentSuggestionId,
        /// The catalogue entry this beat maps to; `None` for bespoke concepts.
        format_key: Option<String>,
        concept: String,
        /// Why this beat and why now, verbatim from the ranked suggestion —
        /// the queue must show the argument, not a key into it.
        reason: String,
        /// Who the piece would actually reach, clause by clause — the part
        /// that makes the suggestion worth reading, so it rides in the
        /// payload rather than a join away.
        distribution_promise: serde_json::Value,
    },
    /// One proposed campaign arc. Approving is the single creative decision
    /// the band makes about the season: the spine, the horizon, the anchor
    /// it is built around. The beats inside it then surface as ordinary
    /// suggestions under the same policy — the band approved the shape once;
    /// it is not re-asked about every beat that fills it.
    ///
    /// Approving transitions the arc `proposed → approved` (it goes `active`
    /// when its horizon opens). Cancelling retires it: a declined arc is a
    /// taste signal that also puts its anchor in cooldown so the engine does
    /// not re-ask the same campaign next week.
    RaiseContentArc {
        arc_id: ArcId,
        title: String,
        /// What the campaign argues and what the spine does, verbatim from
        /// the proposal — the queue must show the plan, not a key into it.
        summary: String,
        /// ISO dates; `None` only if the proposal carries no horizon, which
        /// the proposer never emits — the fields stay honest anyway.
        horizon_start: Option<String>,
        horizon_end: Option<String>,
        /// How many dated beats the spine holds — the size of the commitment
        /// being asked for, readable without expanding the snapshot.
        beats: u32,
    },
    /// Work that was committed to and then left undone. One action kind covers
    /// every debt kind on purpose: the ranked queue compares them against each
    /// other, and four look-alike action kinds would only make that harder.
    RaiseGrowthDebt {
        subject_kind: String,
        subject_id: uuid::Uuid,
        debt_kind: GrowthDebtKind,
        recommended_action: String,
        /// How far past its horizon the work is, in basis points. `10_000` is
        /// exactly at the horizon. Measured, never forecast.
        overdue_basis_points: u32,
        /// Tracked items still outstanding, and how many were tracked at all.
        /// Both travel with the action so the operator sees the denominator
        /// rather than a bare count.
        outstanding_items: u32,
        tracked_items: u32,
        priority: u16,
        template_key: String,
    },
    /// Deliver one step of a running play to one consented fan.
    ///
    /// One recipient per action on purpose. It makes the send idempotent on a
    /// key nobody has to invent (`play + step + fan`), it lets the daily quota
    /// and the weekly envelope bound the campaign in the units they are
    /// written in, and it means a play that goes wrong costs one message before
    /// somebody can stop it rather than a whole segment.
    RunPlayStep {
        play_id: PlayId,
        play_kind: PlayKind,
        step_index: u16,
        step_kind: PlayStepKind,
        /// The show the step is anchored to, when the play has one. Carried so
        /// the executor renders the ask about a specific date rather than about
        /// the band in general; absent for a play anchored on a fan, which has
        /// no date to talk about.
        event_id: Option<EventId>,
        /// Absent for a step with no audience. A listing sweep is work on our
        /// own surfaces and has no recipient; carrying a fan there would be a
        /// contact nobody made.
        fan_id: Option<FanId>,
        template_key: String,
    },
    SendTeamAssignmentEmail {
        assignment_id: uuid::Uuid,
        recipient_email: String,
        recipient_name: String,
        task_title: String,
        task_detail: String,
        due_at: Option<time::OffsetDateTime>,
        action_url_path: String,
        reminder_number: u8,
    },
    /// An LLM agent produced a content draft (press pitch, social post, etc.)
    /// that the operator approved. Execution materializes the draft into the
    /// appropriate channel via the agent.content executor capability — the
    /// agent itself never sends anything, the autopilot does, after approval.
    RequestAgentContent {
        #[serde(default)]
        template_id: Option<String>,
        task_id: uuid::Uuid,
        draft: serde_json::Value,
        /// Who the draft is addressed to, when it is addressed to anybody.
        ///
        /// A channel draft has none — its executor claims it by template. A
        /// press pitch is an email to a named journalist, and without an
        /// address it is a document nobody receives, which is how every press
        /// pitch in production ended. `recipient_target_id` is the
        /// `agent_outreach_targets` row, so a reply can be attributed back.
        /// Defaulted so existing rows deserialize unchanged.
        #[serde(default)]
        recipient_email: Option<String>,
        #[serde(default)]
        recipient_name: Option<String>,
        #[serde(default)]
        recipient_target_id: Option<uuid::Uuid>,
    },
    /// An agent discovered an outreach target (press contact, radio station,
    /// community subreddit) that the operator should verify before the growth
    /// loop engages it. Execution promotes the target from `proposed` to
    /// `promoted` in the `agent_outreach_targets` staging table — an internal
    /// database operation, no external executor needed.
    RequestOutreachTarget {
        task_id: uuid::Uuid,
        target_kind: String,
        display_name: String,
        #[serde(default)]
        contact_email: Option<String>,
        #[serde(default)]
        contact_domain: Option<String>,
        #[serde(default)]
        why_fit: String,
        #[serde(default)]
        evidence_urls: serde_json::Value,
        #[serde(default)]
        subreddit: Option<String>,
    },
    /// The deterministic brain dispatches an LLM worker to gather intelligence
    /// or draft content. The brain decides what to gather and when — never the
    /// LLM. The worker runs the specified template with the deterministic prompt
    /// and emits outcomes that the brain consumes via `AgentOutcomeWorker`.
    RequestAgentRun {
        template_id: String,
        prompt: String,
        priority: u8,
        /// Intelligent token optimization: "basic" routes to free-tier models,
        /// "premium" routes to connected paid providers (Claude, GPT-4o, GLM,
        /// Devin). The brain classifies each task based on stakes and complexity.
        tier: AgentTier,
    },
    /// A community engagement post drafted by the `community-engager` worker
    /// and approved by the operator. Execution emits an outbox event for the
    /// configured executor to post to the external platform (e.g. Reddit) via
    /// the agents service browser session. The autopilot never calls the
    /// platform API directly — it follows the same ThirdParty outbox pattern
    /// as outreach and booking.
    RequestCommunityEngagement {
        target_id: uuid::Uuid,
        platform: String,
        subreddit: Option<String>,
        title: String,
        body: String,
        smart_link: Option<String>,
    },
    /// A Signal push notification drafted by the `signal-inviter` worker and
    /// approved by the operator. Execution inserts `fan_push_deliveries` rows
    /// for consented fans with active push endpoints. The PushDeliveryWorker
    /// then sends them via FCM/Web Push. A sent push cannot be unsent, so this
    /// is OwnedAudience — fans who opted in, not strangers.
    RequestSignalPush {
        task_id: uuid::Uuid,
        title: String,
        body: String,
        target_path: Option<String>,
        event_id: Option<uuid::Uuid>,
        segment: Option<String>,
        /// How many fans the push reaches, when the raise knew (O.5's
        /// audience answer applied to pushes). `None` is honest rather than
        /// convenient: payloads raised before the count existed, or where a
        /// segment could not be measured, say so instead of printing a zero
        /// that would read as "reaches nobody".
        #[serde(default)]
        audience_size: Option<u32>,
        /// Who those people are, in a sentence — "fans with notifications
        /// on who consented to marketing". An operator approving a push to
        /// people's pockets should not have to decode a segment slug to
        /// learn who is in it.
        #[serde(default)]
        audience_basis: String,
    },
}

impl AutopilotActionPayload {
    /// What this action costs and how far its effects reach.
    ///
    /// Exhaustive on purpose: a new payload variant must not compile until
    /// somebody has decided whether the agent may take it unattended. A lookup
    /// table keyed by `action_kind` would silently default a new action to
    /// whatever the fallback was, which is exactly the mistake this ceiling
    /// exists to prevent.
    #[must_use]
    pub const fn action_class(&self) -> ActionClass {
        match self {
            // Money. Ticket and merch prices are here because changing what a
            // customer pays is not recoverable by changing it back — somebody
            // already paid the other number.
            Self::ChangeTicketPrice { .. }
            | Self::ChangeTicketCapacity { .. }
            | Self::ChangeMerchPrice { .. }
            | Self::RequestMerchBundle { .. }
            | Self::RequestMerchReorder { .. }
            | Self::RequestPromotionBudgetChange { .. } => ActionClass::Paid,

            // Somebody else's relationship, and the band gets one first
            // approach to each of them.
            Self::RequestBookingOutreach { .. }
            | Self::RequestGigOutreach { .. }
            | Self::RequestOutreach { .. }
            | Self::RequestRepresentationApproach { .. }
            // The agent application is somebody else's relationship in the
            // most literal sense — they sell *us*, and a bad first approach
            // spends a season, not a cooldown.
            | Self::RequestBookingAgentApproach { .. }
            | Self::RequestBeaconOutreach { .. }
            // An invitation is a letter to somebody outside the band's own
            // audience — that is the whole point of it — so it carries the
            // third-party class, its hold window and its identical-draft
            // refusal.
            | Self::RequestLatarnikInvite { .. }
            // A partner being asked to carry invite codes is a real-world
            // approach to somebody else's community, not a message to ours.
            | Self::RequestBeaconInviteBatch { .. }
            | Self::ApplyLiveOpportunity { .. }
            // A counter and an acceptance are both statements to somebody
            // outside the workspace, and an acceptance is a commitment of the
            // band's calendar and money. Neither is ours to take back.
            | Self::CounterLiveOpportunityTerms { .. }
            | Self::AcceptLiveOpportunityTerms { .. }
            | Self::SubmitFundingApplication { .. }
            // A community post reaches somebody else's platform — Reddit,
            // forums — and once posted it cannot be unsent. The operator
            // approves before it goes, same as any other outward approach.
            | Self::RequestCommunityEngagement { .. } => ActionClass::ThirdParty,

            // Fans who opted in. Free, but a sent message cannot be unsent.
            Self::RequestFanLifecycleMessage { .. }
            | Self::RequestAudienceCampaign { .. }
            | Self::RequestSignalPush { .. } => ActionClass::OwnedAudience,

            // Ours, free and undoable by doing the opposite. The team
            // assignment email is here deliberately: it reaches our own staff,
            // not an audience or a stranger, and treating internal task routing
            // as outward contact would spend the audience budget on ourselves.
            Self::RequestBeaconDiscovery { .. }
            | Self::RequestOutreachDiscovery { .. }
            | Self::RequestBookingTargetDiscovery { .. }
            | Self::RequestContentArtifact { .. }
            | Self::AdjustExperiment { .. }
            | Self::CompleteShowTask { .. }
            | Self::EscalateShowTask { .. }
            | Self::PrepareFundingPackage { .. }
            | Self::RaiseGrowthOpportunity { .. }
            | Self::RaiseGrowthDebt { .. }
            // The advisory itself reaches nobody; approving it flips one
            // discovery_places row to `not_a_fit`, and flipping it back
            // undoes the park — first-party and reversible either way.
            | Self::RaiseDeclineAdvisory { .. }
            | Self::IssueReferralCode { .. }
            // Raising a suggestion or an arc flips one row inside the
            // workspace. It reaches nobody — the promises they name are
            // carried out by separately classed actions, each gated on its
            // own reach.
            | Self::RaiseContentSuggestion { .. }
            | Self::RaiseContentArc { .. }
            | Self::SendTeamAssignmentEmail { .. }
            // A public read. It contacts nobody and changes nothing, which is
            // exactly why it may run unattended: the whole point is checking a
            // claim without asking the person who made it.
            | Self::VerifyPlaylistPlacement { .. }
            | Self::EscalateEditorialPitch { .. }
            // A target promotion is an internal DB write (flipping a staging
            // row from proposed to promoted). It reaches nobody and is undone
            // by flipping the status back.
            | Self::RequestOutreachTarget { .. }
            // The brain dispatching a worker is an internal DB write (creating
            // an agent_service_tasks row). It reaches nobody, costs nothing,
            // and is undone by deleting the task row.
            | Self::RequestAgentRun { .. } => ActionClass::FirstPartyReversible,

            // A channel draft materializes inside the workspace; a draft with
            // a recipient is the send itself — an email to a person outside
            // it. A press pitch that classed itself first-party spent nothing
            // from the outward budget and skipped the evidence gate, which is
            // exactly the hole the gate exists to close.
            Self::RequestAgentContent {
                recipient_email,
                recipient_target_id,
                ..
            } => {
                if recipient_email.is_some() || recipient_target_id.is_some() {
                    ActionClass::ThirdParty
                } else {
                    ActionClass::FirstPartyReversible
                }
            }

            // The step kind decides, not the play and not this table: the same
            // play may legitimately hold an owned-audience ask and a curator
            // approach, and collapsing them to one class would either gate the
            // fan message or let the curator one out unattended.
            Self::RunPlayStep { step_kind, .. } => step_kind.action_class(),

            // These two carry their own reach inside the payload, so one class
            // for the whole variant would be wrong in both directions: it would
            // either gate a push to our own fans or let a press approach go out
            // unattended.
            Self::RequestShowGrowth { lever, .. } => lever.action_class(),
            Self::ExecuteReleaseMilestone { milestone, .. } => match milestone {
                ReleaseMilestone::StartPress => ActionClass::ThirdParty,
                ReleaseMilestone::Announcement
                | ReleaseMilestone::FanWarmup
                | ReleaseMilestone::Countdown
                | ReleaseMilestone::ReleaseDay
                | ReleaseMilestone::Sustain
                // The wrap's missed-it wave is a real send to consented fans,
                // not an internal note — it spends from the same budget and
                // owes the show-week hold like every other outward rung.
                | ReleaseMilestone::Wrap
                // The rotation writes to the same consented owned audience —
                // it spends from the same budget, never beside it (§4i-4).
                | ReleaseMilestone::CatalogueRotation => ActionClass::OwnedAudience,
                // Parking the editorial pitch writes a task inside the
                // workspace. It reaches nobody: the form itself is a human's to
                // submit, and the agent never claims otherwise.
                ReleaseMilestone::SeedCalendar
                | ReleaseMilestone::EditorialPitch => ActionClass::FirstPartyReversible,
            },
        }
    }
}

mod briefing_locale;
pub use briefing_locale::BriefingLocale;

include!("model/action_kind.rs");
include!("model/briefing.rs");

/// Formats a minor-currency amount as a human-readable string.
/// Assumes the amount is in the workspace's currency; the label is neutral
/// because the currency code is not available in the payload.
fn format_minor(minor: i64) -> String {
    let abs = minor.unsigned_abs();
    let whole = abs / 100;
    let cents = abs % 100;
    if minor < 0 {
        format!("-{}.{:02}", whole, cents)
    } else {
        format!("{}.{:02}", whole, cents)
    }
}

/// Maps a `MetricPlatform` to a human-readable Polish label.
fn platform_label(platform: &MetricPlatform) -> String {
    match platform {
        MetricPlatform::Spotify => "Spotify".into(),
        MetricPlatform::Bandsintown => "Bandsintown".into(),
        MetricPlatform::Social => "Social media".into(),
        MetricPlatform::Website => "Strona www".into(),
        MetricPlatform::Ticketing => "Ticketing".into(),
        MetricPlatform::Signal => "Signal".into(),
        MetricPlatform::Merch => "Merch".into(),
        MetricPlatform::YouTube => "YouTube".into(),
        MetricPlatform::TikTok => "TikTok".into(),
        MetricPlatform::SoundCloud => "SoundCloud".into(),
        MetricPlatform::Instagram => "Instagram".into(),
        MetricPlatform::Facebook => "Facebook".into(),
        MetricPlatform::Discord => "Discord".into(),
        MetricPlatform::Telegram => "Telegram".into(),
        MetricPlatform::LastFm => "Last.fm".into(),
        MetricPlatform::Deezer => "Deezer".into(),
        MetricPlatform::Discogs => "Discogs".into(),
        MetricPlatform::Bluesky => "Bluesky".into(),
        MetricPlatform::Bandcamp => "Bandcamp".into(),
        MetricPlatform::X => "X".into(),
    }
}

/// Renders a suggestion's distribution promise as one readable line.
///
/// The promise is clause-keyed JSON — `{"communities": ["r/Metal", ...],
/// "press_contacts": 11, "consented_fans": 340, "peer_audience": [...]}` —
/// and the queue must show *who* the piece reaches, not a count of clauses.
/// A clause that is absent was not promised; a clause that is present gets
/// names, because "these four communities" is the argument and "4" is not.
fn promise_to_text(promise: &serde_json::Value) -> String {
    let Some(map) = promise.as_object() else {
        return "—".to_owned();
    };
    let mut clauses: Vec<String> = Vec::new();
    if let Some(communities) = map
        .get("communities")
        .and_then(serde_json::Value::as_array)
        .filter(|list| !list.is_empty())
    {
        let names: Vec<&str> = communities.iter().filter_map(|v| v.as_str()).collect();
        clauses.push(format!(
            "{} communities ({})",
            names.len(),
            names.join(", ")
        ));
    }
    if let Some(press) = map
        .get("press_contacts")
        .and_then(serde_json::Value::as_u64)
    {
        clauses.push(format!("{press} press contacts"));
    }
    if let Some(fans) = map
        .get("consented_fans")
        .and_then(serde_json::Value::as_u64)
    {
        clauses.push(format!("{fans} consented fans"));
    }
    if let Some(peers) = map
        .get("peer_audience")
        .and_then(serde_json::Value::as_array)
        .filter(|list| !list.is_empty())
    {
        let names: Vec<&str> = peers.iter().filter_map(|v| v.as_str()).collect();
        clauses.push(format!("peer audiences ({})", names.join(", ")));
    }
    if clauses.is_empty() {
        "—".to_owned()
    } else {
        clauses.join("; ")
    }
}

/// Extracts readable text from a draft JSON value.
/// Drafts are structured JSON from the agent service; this flattens them
/// into a single string for display. Falls back to pretty-printed JSON.
fn draft_to_text(draft: &serde_json::Value) -> String {
    if draft.is_null() {
        return "(The agent produced no content — open the task in the console for details)"
            .to_owned();
    }
    if let Some(s) = draft.as_str() {
        return s.to_owned();
    }
    if let Some(obj) = draft.as_object() {
        let mut parts = Vec::new();
        for (key, val) in obj {
            if let Some(s) = val.as_str() {
                parts.push(format!("{}: {}", key, s));
            } else {
                parts.push(format!("{}: {}", key, val));
            }
        }
        return parts.join("\n");
    }
    serde_json::to_string_pretty(draft).unwrap_or_default()
}

/// Action-ready decision emitted by application orchestration.
#[derive(Clone, Debug, Serialize)]
pub struct DecisionCandidate {
    #[serde(skip)]
    pub context: AutopilotContext,
    #[serde(skip)]
    pub subject: ActionSubject,
    pub decision_kind: &'static str,
    pub confidence: Confidence,
    pub disposition: PolicyDisposition,
    pub reason: &'static str,
    pub input_snapshot: serde_json::Value,
    pub policy_snapshot: serde_json::Value,
    pub action: AutopilotActionPayload,
    /// Dedupe key for equivalent evidence. Changes when relevant input or policy changes.
    pub decision_key: String,
    /// Stable side-effect key. Intentionally independent from decision history.
    pub action_idempotency_key: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CandidatePersistence {
    pub decision_created: bool,
    pub action_created: bool,
    pub quota_throttled: bool,
    /// The action ID, if an action was created or already existed.
    /// Used by the growth intelligence evaluator to record dispatch
    /// predictions linked to the action for later measurement comparison.
    pub action_id: Option<uuid::Uuid>,
}

/// One kind of play, its measured record and what that record is allowed to
/// change about it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PlayKindStanding {
    pub kind: PlayKind,
    pub record: OutcomeRecord,
    pub standing: Standing,
    /// The operator's ceiling narrowed by the record. Never widened: a perfect
    /// record still reaches exactly the number an operator configured.
    pub effective_max_recipients_per_step: u32,
}

/// One kind of outreach target, its measured record and what that record is
/// allowed to change about wave sizing for that kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OutreachKindStanding {
    pub kind: OutreachTargetKind,
    pub record: OutcomeRecord,
    pub standing: Standing,
    /// The operator's wave-size ceiling narrowed by the record.
    pub effective_max_pitches_per_wave: u32,
}

/// A fact the agent could hang a campaign on, before any play exists for it.
///
/// Read separately from running plays because there is no state machine yet:
/// this is an anchor being considered, not a play being advanced.
/// The specific thing a play hangs off.
///
/// Carried as one value rather than an id plus a kind, because those two can
/// disagree: an event id read as a fan is a play whose audience query returns
/// nothing for ever, and nothing about that failure looks like a bug.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlayAnchorRef {
    Event { event_id: EventId },
    Fan { fan_id: FanId },
    Release { release_plan_id: ReleasePlanId },
}

impl PlayAnchorRef {
    #[must_use]
    pub const fn kind(self) -> PlayAnchorKind {
        match self {
            Self::Event { .. } => PlayAnchorKind::Event,
            Self::Fan { .. } => PlayAnchorKind::Fan,
            Self::Release { .. } => PlayAnchorKind::Release,
        }
    }

    #[must_use]
    pub fn id(self) -> uuid::Uuid {
        match self {
            Self::Event { event_id } => event_id.into_uuid(),
            Self::Fan { fan_id } => fan_id.into_uuid(),
            Self::Release { release_plan_id } => release_plan_id.into_uuid(),
        }
    }

    /// The show, when there is one. A fan-anchored play has no date to render.
    #[must_use]
    pub const fn event_id(self) -> Option<EventId> {
        match self {
            Self::Event { event_id } => Some(event_id),
            Self::Fan { .. } | Self::Release { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayAnchor {
    pub anchor: PlayAnchorRef,
    pub anchor_at: OffsetDateTime,
    /// False for a show that is cancelled or no longer published, or a fan who
    /// is no longer contactable. Carried rather than filtered away in SQL so
    /// the refusal to start is a domain rule somebody can read, not a `WHERE`
    /// clause somebody can loosen.
    pub active: bool,
    /// Hours from now to the anchor. Zero or negative for an anchor that has
    /// already happened, which is every fan anchor: the moment they qualified.
    pub hours_until: i64,
}

/// One step of a play as it will be written when the play starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayStepPlan {
    pub index: u16,
    pub kind: PlayStepKind,
    pub class: ActionClass,
    pub due_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

/// A play about to be created, with its whole schedule already resolved.
///
/// Every step's window is derived from the anchor here, once, and then stored.
/// A play whose schedule were recomputed each cycle would silently reschedule
/// itself whenever the offsets in the code changed, and a campaign that moves
/// under a running send is not auditable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayStart {
    pub kind: PlayKind,
    pub anchor: PlayAnchorRef,
    pub anchor_at: OffsetDateTime,
    pub hypothesis: &'static str,
    pub success_metric_platform: &'static str,
    pub success_metric_key: &'static str,
    pub steps: Vec<PlayStepPlan>,
    /// When the play's effect may first be read: after its last step closes,
    /// plus the settle period. Carried on the start because the baseline is
    /// frozen in the same transaction that creates the play — a baseline
    /// computed later would be read from a series the play has already moved.
    pub measurement_window_end: OffsetDateTime,
}

/// Who is left for the open step of a play.
///
/// One type rather than a count plus an optional id, because those two can
/// disagree: a positive count with no recipient would make the play claim work
/// it cannot do, and the disagreement would only surface as a play that holds
/// for ever.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayAudience {
    /// Nobody eligible is left. The step has done all it can.
    Exhausted,
    /// The step needs nobody. Distinct from `Exhausted`, which means the step
    /// wanted an audience and ran out of one — and settles the play, where this
    /// lets it run.
    NotRequired,
    /// The next fan to reach, and how many remain including them.
    Next { fan_id: FanId, remaining: u32 },
}

impl PlayAudience {
    #[must_use]
    pub const fn remaining(self) -> u32 {
        match self {
            Self::Exhausted => 0,
            // One, so the state machine sees work rather than an empty segment.
            // The step's own ceiling of one is what stops it running twice.
            Self::NotRequired => 1,
            Self::Next { remaining, .. } => remaining,
        }
    }

    #[must_use]
    pub const fn fan_id(self) -> Option<FanId> {
        match self {
            Self::Exhausted | Self::NotRequired => None,
            Self::Next { fan_id, .. } => Some(fan_id),
        }
    }
}

/// One running play, as the cycle reads it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayRunSnapshot {
    pub play_id: PlayId,
    pub kind: PlayKind,
    pub anchor: PlayAnchorRef,
    pub anchor_at: OffsetDateTime,
    pub anchor_active: bool,
    pub steps: Vec<PlayStepState>,
    pub audience: PlayAudience,
}

/// One claimed placement, with the public identifiers a read needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistPlacementSnapshot {
    pub placement: PlacementSnapshot,
    pub playlist_external_id: String,
    pub track_external_id: String,
}

/// A curator's claim, or the result of one public read of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordPlaylistPlacement {
    pub opportunity_id: OutreachOpportunityId,
    pub playlist_external_id: String,
    pub track_external_id: String,
    /// Absent when this is the curator's claim arriving. Present when it is a
    /// read reporting what it found.
    pub observation: Option<PlacementObservation>,
}

/// Ending a placement, and whether the curator behind it is finished with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacementSettlement {
    pub opportunity_id: OutreachOpportunityId,
    pub state: PlacementState,
}

/// One free-reach wave as the cycle reads it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutreachWaveSnapshot {
    pub wave_id: uuid::Uuid,
    pub snapshot: crowdrelay_domain::free_reach::WaveSnapshot,
}

/// An anchor that has no wave of this kind yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutreachWaveAnchor {
    pub anchor: WaveAnchor,
    pub anchor_at: OffsetDateTime,
    pub target_kind: OutreachTargetKind,
    pub active: bool,
    pub hours_until: i64,
    /// Targets of this kind that would pass the outreach rules right now.
    pub eligible_targets: u32,
}

/// A wave about to be created, with the ceiling it was sized against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutreachWaveStart {
    pub anchor: WaveAnchor,
    pub anchor_at: OffsetDateTime,
    pub target_kind: OutreachTargetKind,
    /// Frozen at open, so an operator reading a sealed wave sees the budget it
    /// was drafted under rather than today's.
    pub capacity: u16,
}

/// Closing a wave, either for review or for good.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutreachWaveTransition {
    /// Closed for changes and put in front of a human.
    Seal,
    /// Ended without being approved, for a stated reason.
    Expire { reason: WaveExpiry },
}

/// The first-party numbers a pitch is allowed to claim.
///
/// Every field is optional and every one is omitted rather than defaulted when
/// the workspace cannot answer it. A zero the agent invented reads exactly like
/// a zero it measured, and the difference is the whole point: this exists so a
/// pitch carries numbers instead of adjectives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct EvidencePacket {
    /// The success series the workspace watches, and how fast it is moving.
    pub trackers: Option<i64>,
    pub trackers_per_day_milli: Option<i64>,
    /// Paid tickets in the last ninety days. Real money from real people, which
    /// is the only number in here a curator has no reason to discount.
    pub paid_tickets_90d: Option<i64>,
    /// Shows actually played in the last year.
    pub shows_played_12m: Option<i64>,
    /// Relationships that have replied positively before. Coverage we can point
    /// at rather than coverage we hope for.
    pub positive_replies_12m: Option<i64>,
    /// When these were read. A number without one is a number from any time.
    pub as_of: Option<OffsetDateTime>,
}

/// One live negotiation, with the opportunity it is about.
///
/// Carried together because every rule in `crowdrelay_domain::negotiation`
/// needs both: the ladder comes from the terms row and the refusals come from
/// the show. Read apart, they would be two moments' answers to one question.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveTermsSnapshot {
    pub terms: TermsSnapshot,
    pub opportunity: LiveOpportunitySnapshot,
    /// The negotiation's currency, carried from the row rather than assumed:
    /// a counter quoted in the wrong one is a different offer.
    pub currency: String,
}

/// Ending a negotiation without an acceptance, and why.
///
/// Not an action: the agent records that it will not take these terms, and
/// telling the promoter stays a human act. A declined row an operator can read
/// is the point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TermsSettlement {
    pub opportunity_id: TeamOpportunityId,
    pub state: TermsState,
    pub reason: Option<TermsRefusal>,
}

/// Settling a step without delivering it, and why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayStepSettlement {
    pub play_id: PlayId,
    pub step_index: u16,
    pub reason: StepSkipReason,
}

/// One claim about one play, claimed for settlement by the worker.
#[derive(Clone, Debug)]
pub struct ClaimedPlayOutcome {
    pub id: uuid::Uuid,
    pub play_id: PlayId,
    pub kind: PlayKind,
    pub claim: PlayClaim,
    pub success_metric_platform: String,
    pub success_metric_key: String,
    /// Frozen when the play started. `None` when the series had no usable trend
    /// then, which settles as `no_baseline` rather than as zero.
    pub baseline_value: Option<i64>,
    pub baseline_milli_per_day: Option<i64>,
    pub window_start: OffsetDateTime,
    pub window_end: OffsetDateTime,
    pub attempt_number: u32,
}

/// What the window actually holds, read once when the outcome settles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayOutcomeObservation {
    pub observed_at: OffsetDateTime,
    pub observed_value: Option<i64>,
    pub observed_milli_per_day: Option<i64>,
    /// Fans the play reached, from the delivered-recipient rows rather than
    /// from the actions it created. An effect needs a denominator, and the
    /// denominator is who actually heard from the band.
    pub recipients_reached: u32,
    /// Clicks our own rows join to this play. `None` means no join key exists —
    /// a different fact from zero clicks, and the difference is the whole
    /// separation between the two claims.
    pub attributed_clicks: Option<i64>,
    pub direction: MetricDirection,
    pub ambiguous_series: bool,
}

/// Action claimed for execution by the worker.
#[derive(Clone, Debug)]
pub struct ClaimedAutopilotAction {
    pub id: AutopilotActionId,
    pub payload: AutopilotActionPayload,
    pub attempt_number: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T12: TargetCommunity action subject has correct kind and uuid.
    #[test]
    fn t12_target_community_subject() {
        let target_id = uuid::Uuid::now_v7();
        let subject = ActionSubject::TargetCommunity(target_id);
        assert_eq!(subject.kind(), "target_community");
        assert_eq!(subject.uuid(), target_id);
        assert!(!subject.is_contactable_person());
    }

    /// T13: TargetCommunity and Workspace have different audience keys.
    /// The audience_key_for function in portfolio.rs extracts the
    /// target_id from community-engager decision_keys, producing
    /// "community:{target_id}". Workspace-wide templates produce
    /// "workspace:{workspace_id}". Two different templates targeting
    /// the same community share the same audience_key.
    #[test]
    fn t13_target_community_audience_is_target_not_template() {
        let target_id = uuid::Uuid::now_v7();
        let subject = ActionSubject::TargetCommunity(target_id);
        // The subject's UUID is the target_id, not the workspace_id.
        // This means the experiment unit is the community, not the workspace.
        assert_ne!(subject.kind(), "workspace");
    }

    /// A workspace-scoped debt finding maps onto the workspace action subject
    /// so cooldown, dedup, and the operator queue all key off the workspace id.
    #[test]
    fn workspace_debt_subject_maps_to_workspace_action_subject() {
        let workspace_id = WorkspaceId::new();
        let subject = ActionSubject::from(
            crowdrelay_domain::growth_debt::GrowthDebtSubject::Workspace(workspace_id),
        );
        assert_eq!(subject, ActionSubject::Workspace(workspace_id));
        assert_eq!(subject.kind(), "workspace");
        assert_eq!(subject.uuid(), workspace_id.into_uuid());
    }

    /// The content-artifact briefing shows the operator-facing names, not the
    /// machine keys: the artifact kind is a label and the template contract
    /// loses its `content.` namespace but keeps its version.
    #[test]
    fn content_artifact_briefing_shows_names_not_keys() {
        let briefing = AutopilotActionPayload::RequestContentArtifact {
            source_id: ContentSourceId::new(),
            source_version: 1,
            artifact: ContentArtifactKind::SocialFeed,
            template_key: "content.social_feed.v1".to_owned(),
        }
        .briefing();

        assert_eq!(briefing.summary, "Content artifact: Social feed");
        let field = |label: &str| {
            briefing
                .content
                .iter()
                .find(|f| f.label == label)
                .map(|f| f.value.as_str())
        };
        assert_eq!(field("Artifact"), Some("Social feed"));
        assert_eq!(field("Template"), Some("Social feed v1"));
    }

    /// §12-6: the booking payload round-trips with its window, recipient set
    /// and evidence row intact — the whole proposal is persisted as JSONB.
    #[test]
    fn booking_outreach_payload_round_trips_window_recipients_and_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::booking::BookingVenueEvidence;
        use crowdrelay_domain::booking_window::{ProposedWindow, WindowBasis};
        let day = time::Date::from_calendar_date(2026, time::Month::April, 10)?;
        let payload = AutopilotActionPayload::RequestBookingOutreach {
            city_id: CityId::new(),
            target_id: BookingTargetId::new(),
            target_version: 3,
            target_name: "Klub X".to_owned(),
            score: 71,
            phase: BookingOutreachPhase::Initial,
            proposed_window: Some(ProposedWindow {
                start: day,
                end: day + time::Duration::days(28),
                basis: vec![
                    WindowBasis::LeadTime { median_days: 42 },
                    WindowBasis::RoomRhythm {
                        median_gap_days: 28,
                    },
                    WindowBasis::AdjacentShow {
                        event_slug: "krakow-night".to_owned(),
                        distance_km: 252,
                    },
                ],
            }),
            additional_recipients: vec![(BookingTargetId::new(), 2)],
            venue_evidence: Some(BookingVenueEvidence {
                shows_last_12m: 9,
                comparable_acts: 3,
                genres: Some("metal, rock".to_owned()),
                capacity: Some("300".to_owned()),
                days_since_last_event: Some(11),
                booking_contact_days: Some(18),
            }),
        };
        let json = serde_json::to_value(&payload)?;
        let back: AutopilotActionPayload = serde_json::from_value(json.clone())?;
        assert_eq!(back, payload);
        // The outward-facing shape the spec asks for.
        assert_eq!(json["kind"], serde_json::json!("request_booking_outreach"));
        assert_eq!(
            json["additional_recipients"].as_array().map(Vec::len),
            Some(1)
        );
        Ok(())
    }

    /// Payloads written before the window existed must still parse — the new
    /// fields are all serde-defaulted.
    #[test]
    fn booking_outreach_payload_without_window_still_parses()
    -> Result<(), Box<dyn std::error::Error>> {
        let legacy = serde_json::json!({
            "kind": "request_booking_outreach",
            "city_id": uuid::Uuid::now_v7(),
            "target_id": uuid::Uuid::now_v7(),
            "target_version": 1,
            "target_name": "Klub Y",
            "score": 60,
            "phase": "initial",
        });
        let AutopilotActionPayload::RequestBookingOutreach {
            proposed_window,
            additional_recipients,
            venue_evidence,
            ..
        } = serde_json::from_value(legacy)?
        else {
            panic!("the legacy payload must still parse as RequestBookingOutreach")
        };
        assert_eq!(proposed_window, None);
        assert!(additional_recipients.is_empty());
        assert_eq!(venue_evidence, None);
        Ok(())
    }
}
