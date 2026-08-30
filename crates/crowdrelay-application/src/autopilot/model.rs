//! Stable application-boundary types for ViryaOS Autopilot.

use crowdrelay_brain::{AgentTier, GrowthIntelligencePolicy};
use crowdrelay_domain::{
    AutopilotActionId, BeaconId, BookingTargetId, CityId, ContentSourceId, EventId, ExperimentId,
    ExperimentVariantId, FanId, GrowthMetricSeriesId, MerchProductId, MerchVariantId,
    OutreachOpportunityId, OutreachTargetId, PlayId, PromotionCampaignId, ReleasePlanId,
    TeamOpportunityId, TicketTypeId, WorkspaceId,
    action_class::ActionClass,
    audience_lifecycle::FanLifecyclePolicy,
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    beacons::{BeaconCampaignPolicy, BeaconOutreachPhase},
    booking::{BookingOpportunityPolicy, BookingOutreachPhase},
    campaign_lifecycle::{EventCampaignPhase, EventCampaignPolicy},
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
}

impl AutopilotContext {
    /// Every context, in policy-store order.
    ///
    /// Storage parsing is derived from this list rather than restating the
    /// names: a context the policy table can hold but a reader cannot parse
    /// fails the whole overview read, not just its own row.
    pub const ALL: [Self; 22] = [
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
                Self::parse_into(raw, Self::ShowOperations, ShowOperationsPolicy::default())
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
    OutreachTarget(OutreachTargetId),
    /// P0-3: A community target discovered by the agent (e.g. a subreddit
    /// from `agent_outreach_targets`). Distinct from `OutreachTarget`
    /// which is an operator-managed contact in `viryaos_outreach_targets`.
    /// The UUID is `agent_outreach_targets.id`.
    TargetCommunity(uuid::Uuid),
    /// Supply is a property of the whole workspace rather than of any one row,
    /// so the sweep that replenishes it has the workspace as its subject.
    Workspace(WorkspaceId),
}

impl From<GrowthDebtSubject> for ActionSubject {
    fn from(subject: GrowthDebtSubject) -> Self {
        match subject {
            GrowthDebtSubject::BookingTarget(id) => Self::BookingTarget(id),
            GrowthDebtSubject::OutreachTarget(id) => Self::OutreachTarget(id),
            GrowthDebtSubject::Beacon(id) => Self::Beacon(id),
            GrowthDebtSubject::Event(id) => Self::Event(id),
            GrowthDebtSubject::ReleasePlan(id) => Self::ReleasePlan(id),
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
            Self::OutreachTarget(_) => "outreach_target",
            Self::TargetCommunity(_) => "target_community",
            Self::Workspace(_) => "workspace",
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
            Self::Fan(_) | Self::BookingTarget(_) | Self::OutreachTarget(_) | Self::Beacon(_)
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
            Self::OutreachTarget(id) => id.into_uuid(),
            Self::TargetCommunity(id) => id,
            Self::Workspace(id) => id.into_uuid(),
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
    },
    RequestAudienceCampaign {
        event_id: EventId,
        phase: EventCampaignPhase,
        template_key: String,
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
    /// Give a consented fan a referral code.
    ///
    /// The only growth mechanism that scales with the audience rather than with
    /// the band's effort, and it has to exist before anything invites anybody.
    IssueReferralCode {
        fan_id: FanId,
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
            | Self::RequestOutreach { .. }
            | Self::RequestBeaconOutreach { .. }
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
            | Self::IssueReferralCode { .. }
            | Self::SendTeamAssignmentEmail { .. }
            // A public read. It contacts nobody and changes nothing, which is
            // exactly why it may run unattended: the whole point is checking a
            // claim without asking the person who made it.
            | Self::VerifyPlaylistPlacement { .. }
            | Self::EscalateEditorialPitch { .. }
            // An LLM draft materializes as a first-party campaign draft; the
            // actual send is a separate, separately-approved action.
            | Self::RequestAgentContent { .. }
            // The brain dispatching a worker is an internal DB write (creating
            // an agent_service_tasks row). It reaches nobody, costs nothing,
            // and is undone by deleting the task row.
            | Self::RequestAgentRun { .. } => ActionClass::FirstPartyReversible,

            // The step kind decides, not the play and not this table: the same
            // play may legitimately hold an owned-audience ask and a curator
            // approach, and collapsing them to one class would either gate the
            // fan message or let the curator one out unattended.
            Self::RunPlayStep { step_kind, .. } => step_kind.action_class(),

            // These two carry their own reach inside the payload, so one class
            // for the whole variant would be wrong in both directions: it would
            // either gate a push to our own fans or let a press approach go out
            // unattended.
            Self::RequestShowGrowth { lever, .. } => match lever {
                ShowGrowthLever::PartnerCrossPromo
                | ShowGrowthLever::GrassrootsSceneRelay
                | ShowGrowthLever::SocialProofRelay => ActionClass::ThirdParty,
                ShowGrowthLever::FanAmbassadors
                | ShowGrowthLever::FreeFanChannelPush
                | ShowGrowthLever::MerchBuyerOffer
                | ShowGrowthLever::HighIntentLastMile
                | ShowGrowthLever::PostShowMerchFollowUp
                | ShowGrowthLever::PostShowFollowAsk => ActionClass::OwnedAudience,
                ShowGrowthLever::CanonicalLinkSetup
                | ShowGrowthLever::FreeListingSweep
                | ShowGrowthLever::AudienceCaptureSetup => ActionClass::FirstPartyReversible,
            },
            Self::ExecuteReleaseMilestone { milestone, .. } => match milestone {
                ReleaseMilestone::StartPress => ActionClass::ThirdParty,
                ReleaseMilestone::Announcement
                | ReleaseMilestone::FanWarmup
                | ReleaseMilestone::Countdown
                | ReleaseMilestone::ReleaseDay
                | ReleaseMilestone::Sustain => ActionClass::OwnedAudience,
                // Parking the editorial pitch writes a task inside the
                // workspace. It reaches nobody: the form itself is a human's to
                // submit, and the agent never claims otherwise.
                ReleaseMilestone::SeedCalendar
                | ReleaseMilestone::EditorialPitch
                | ReleaseMilestone::Wrap => ActionClass::FirstPartyReversible,
            },
        }
    }

    #[must_use]
    pub const fn action_kind(&self) -> &'static str {
        match self {
            Self::ChangeTicketPrice { .. } => "ticket.price.change",
            Self::ChangeTicketCapacity { .. } => "ticket.capacity.change",
            Self::RequestFanLifecycleMessage { .. } => "fan.lifecycle.message.request",
            Self::RequestMerchReorder { .. } => "merch.reorder.request",
            Self::ChangeMerchPrice { .. } => "merch.price.change",
            Self::RequestBookingOutreach { .. } => "booking.outreach.request",
            Self::RequestAudienceCampaign { .. } => "audience.campaign.request",
            Self::RequestMerchBundle { .. } => "merch.bundle.request",
            Self::RequestOutreach { .. } => "outreach.request",
            Self::RequestBeaconDiscovery { .. } => "beacon.discovery.request",
            Self::RequestBookingTargetDiscovery { .. } => "booking.target_discovery.request",
            Self::RequestBeaconInviteBatch { .. } => "beacon.invite_batch.request",
            Self::RequestOutreachDiscovery { .. } => "outreach.discovery.request",
            Self::RequestBeaconOutreach { .. } => "beacon.outreach.request",
            Self::RequestShowGrowth { .. } => "show.growth.request",
            Self::RequestContentArtifact { .. } => "content.artifact.request",
            Self::AdjustExperiment {
                complete: false, ..
            } => "experiment.allocation.change",
            Self::AdjustExperiment { complete: true, .. } => "experiment.complete",
            Self::CompleteShowTask { .. } => "show.task.complete",
            Self::EscalateShowTask { .. } => "show.task.escalate",
            Self::RequestPromotionBudgetChange { .. } => "promotion.budget_change.request",
            Self::ExecuteReleaseMilestone { .. } => "release.milestone.execute",
            Self::ApplyLiveOpportunity { .. } => "opportunity.live.apply",
            Self::VerifyPlaylistPlacement { .. } => "playlist.placement.verify",
            Self::EscalateEditorialPitch { .. } => "release.editorial_pitch.escalate",
            Self::CounterLiveOpportunityTerms { .. } => "opportunity.terms.counter",
            Self::AcceptLiveOpportunityTerms { .. } => "opportunity.terms.accept",
            Self::PrepareFundingPackage { .. } => "funding.package.prepare",
            Self::SubmitFundingApplication { .. } => "funding.application.submit",
            Self::RaiseGrowthOpportunity { .. } => "growth.opportunity.raise",
            Self::RaiseGrowthDebt { .. } => "growth.debt.raise",
            Self::IssueReferralCode { .. } => "referral.code.issue",
            Self::RunPlayStep { .. } => "play.step.run",
            Self::SendTeamAssignmentEmail { .. } => "team.assignment.email",
            Self::RequestAgentContent { .. } => "agent.content.request",
            Self::RequestAgentRun { .. } => "agent.run.request",
            Self::RequestCommunityEngagement { .. } => "community.engage.request",
            Self::RequestSignalPush { .. } => "signal.push.request",
        }
    }

    /// Generates a human-readable briefing for this action — what to do,
    /// why it matters, concrete steps, and the content being approved.
    ///
    /// Exhaustive on purpose: a new payload variant must not compile until
    /// somebody writes its briefing. The `deadline_note` is left empty here
    /// and filled by the caller from the action's deadline fields.
    #[must_use]
    pub fn briefing(&self) -> super::control::ActionBriefing {
        use super::control::{ActionBriefing, BriefingField, BriefingStep};

        let truncate = |s: String, max: usize| {
            if s.len() > max {
                let mut truncated = s.chars().take(max.saturating_sub(1)).collect::<String>();
                truncated.push('…');
                truncated
            } else {
                s
            }
        };

        match self {
            Self::ChangeTicketPrice { ticket_type_id, from_minor, to_minor } => ActionBriefing {
                summary: format!("Zmień cenę biletu: {} → {}", format_minor(*from_minor), format_minor(*to_minor)),
                why_it_matters: "Zmiana ceny biletu wpływa na przychód i frekwencję. Ktoś już mógł zapłacić starą cenę — zmiana nie jest odwracalna.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź nową cenę i typ biletu".into(), why_it_matters: "Upewnij się, że zmiana jest celowa".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zastosować".into(), why_it_matters: "Po akceptacji cena jest aktywna natychmiast".into() },
                ],
                content: vec![
                    BriefingField { label: "Typ biletu".into(), value: ticket_type_id.to_string() },
                    BriefingField { label: "Stara cena".into(), value: format_minor(*from_minor) },
                    BriefingField { label: "Nowa cena".into(), value: format_minor(*to_minor) },
                ],
                deadline_note: String::new(),
            },
            Self::ChangeTicketCapacity { ticket_type_id, from_capacity, to_capacity, .. } => ActionBriefing {
                summary: format!("Zmień pojemność biletów: {} → {}", from_capacity, to_capacity),
                why_it_matters: "Zmiana pojemności wpływa na dostępność biletów. Zmniejszenie może odrzucić zarezerwowane miejsca.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź nową pojemność".into(), why_it_matters: "Upewnij się, że venue to obsłuży".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zastosować".into(), why_it_matters: "Po akceptacji pojemność jest aktywna".into() },
                ],
                content: vec![
                    BriefingField { label: "Typ biletu".into(), value: ticket_type_id.to_string() },
                    BriefingField { label: "Stara pojemność".into(), value: from_capacity.to_string() },
                    BriefingField { label: "Nowa pojemność".into(), value: to_capacity.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestFanLifecycleMessage { fan_id, template_key } => ActionBriefing {
                summary: format!("Wyślij wiadomość do fana: {}", template_key),
                why_it_matters: "Wiadomość trafi do fana, który wyraził zgodę. Wysłanej wiadomości nie można cofnąć.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź treść wiadomości i szablon".into(), why_it_matters: "Upewnij się, że ton jest odpowiedni".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać".into(), why_it_matters: "Wiadomość zostanie dostarczona do fana".into() },
                ],
                content: vec![
                    BriefingField { label: "Fan".into(), value: fan_id.to_string() },
                    BriefingField { label: "Szablon".into(), value: template_key.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestMerchReorder { variant_id, quantity } => ActionBriefing {
                summary: format!("Zamów ponownie merch: {} szt.", quantity),
                why_it_matters: "Zamówienie merchu wymaga kosztów i czasu dostawy. Upewnij się, że zapas jest potrzebny.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź ilość i wariant produktu".into(), why_it_matters: "Potwierdź, że zapas faktycznie brakuje".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zamówić".into(), why_it_matters: "Po akceptacji zamówienie zostanie złożone".into() },
                ],
                content: vec![
                    BriefingField { label: "Wariant".into(), value: variant_id.to_string() },
                    BriefingField { label: "Ilość".into(), value: quantity.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::ChangeMerchPrice { product_id, from_minor, to_minor, .. } => ActionBriefing {
                summary: format!("Zmień cenę merchu: {} → {}", format_minor(*from_minor), format_minor(*to_minor)),
                why_it_matters: "Zmiana ceny merchu wpływa na marżę i sprzedaż. Ktoś już mógł zapłacić starą cenę.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź nową cenę".into(), why_it_matters: "Upewnij się, że zmiana jest celowa".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zastosować".into(), why_it_matters: "Po akceptacji cena jest aktywna natychmiast".into() },
                ],
                content: vec![
                    BriefingField { label: "Produkt".into(), value: product_id.to_string() },
                    BriefingField { label: "Stara cena".into(), value: format_minor(*from_minor) },
                    BriefingField { label: "Nowa cena".into(), value: format_minor(*to_minor) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBookingOutreach { target_name, score, phase, .. } => ActionBriefing {
                summary: format!("Kontakt bookingowy: {}", target_name),
                why_it_matters: "To pierwsze podejście do promotera. Otrzymasz jedną szansę na kontakt — upewnij się, że wiadomość jest dobra.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź nazwę celu i fazę kontaktu".into(), why_it_matters: "Upewnij się, że to właściwy promoter".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać".into(), why_it_matters: "Po akceptacji wiadomość zostanie wysłana".into() },
                ],
                content: vec![
                    BriefingField { label: "Cel".into(), value: target_name.clone() },
                    BriefingField { label: "Faza".into(), value: format!("{:?}", phase) },
                    BriefingField { label: "Wynik".into(), value: format!("{}", score) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestAudienceCampaign { event_id, phase, template_key } => ActionBriefing {
                summary: format!("Kampania audience: {}", template_key),
                why_it_matters: "Kampania trafi do fanów związanych z wydarzeniem. Wysłanej kampanii nie można cofnąć.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź szablon i fazę kampanii".into(), why_it_matters: "Upewnij się, że treść jest odpowiednia dla fazy".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby uruchomić".into(), why_it_matters: "Po akceptacji kampania zostanie wysłana".into() },
                ],
                content: vec![
                    BriefingField { label: "Wydarzenie".into(), value: event_id.to_string() },
                    BriefingField { label: "Faza".into(), value: format!("{:?}", phase) },
                    BriefingField { label: "Szablon".into(), value: template_key.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestMerchBundle { product_a, product_b, bundle_price_minor, affinity_basis_points } => ActionBriefing {
                summary: format!("Utwórz zestaw merch: {}", format_minor(*bundle_price_minor)),
                why_it_matters: "Zestaw łączy dwa produkty w jedną cenę. Upewnij się, że afinitet jest wystarczający.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź produkty i cenę zestawu".into(), why_it_matters: "Upewnij się, że marża jest akceptowalna".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby utworzyć".into(), why_it_matters: "Po akceptacji zestaw będzie dostępny".into() },
                ],
                content: vec![
                    BriefingField { label: "Produkt A".into(), value: product_a.to_string() },
                    BriefingField { label: "Produkt B".into(), value: product_b.to_string() },
                    BriefingField { label: "Cena zestawu".into(), value: format_minor(*bundle_price_minor) },
                    BriefingField { label: "Afinitet".into(), value: format!("{}%", affinity_basis_points / 100) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestOutreach { target_name, phase, template_key, .. } => ActionBriefing {
                summary: format!("Kontakt outreach: {}", target_name),
                why_it_matters: "To podejście do zewnętrznego celu. Otrzymasz jedną szansę na kontakt.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź cel, fazę i szablon".into(), why_it_matters: "Upewnij się, że wiadomość jest spersonalizowana".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać".into(), why_it_matters: "Po akceptacji wiadomość zostanie wysłana".into() },
                ],
                content: vec![
                    BriefingField { label: "Cel".into(), value: target_name.clone() },
                    BriefingField { label: "Faza".into(), value: format!("{:?}", phase) },
                    BriefingField { label: "Szablon".into(), value: template_key.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::VerifyPlaylistPlacement { playlist_external_id, track_external_id, checkpoint, .. } => ActionBriefing {
                summary: format!("Zweryfikuj playlistę (sprawdzenie {})", checkpoint),
                why_it_matters: "Weryfikacja sprawdza czy utwór jest na playliście. To odczyt danych publicznych — nie kontaktuje nikogo.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby uruchomić weryfikację".into(), why_it_matters: "System odczyta publiczną playlistę i sprawdzi utwór".into() },
                ],
                content: vec![
                    BriefingField { label: "Playlist ID".into(), value: playlist_external_id.clone() },
                    BriefingField { label: "Track ID".into(), value: track_external_id.clone() },
                    BriefingField { label: "Punkt kontrolny".into(), value: checkpoint.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBeaconDiscovery { event_id, target_count } => ActionBriefing {
                summary: format!("Znajdź {} lokalne Beacony", target_count),
                why_it_matters: "System przeszuka lokalne Beacony w okolicy wydarzenia. To odczyt danych — nie kontaktuje nikogo.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby uruchomić wyszukiwanie".into(), why_it_matters: "System znajdzie potencjalne Beacony dla wydarzenia".into() },
                ],
                content: vec![
                    BriefingField { label: "Wydarzenie".into(), value: event_id.to_string() },
                    BriefingField { label: "Liczba celów".into(), value: target_count.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBeaconInviteBatch { beacon_id, event_id, requested_count, .. } => ActionBriefing {
                summary: format!("Poproś Beacon o {} kodów zaproszenia", requested_count),
                why_it_matters: "To prośba do partnera (Beacon) o rozdystrybuowanie kodów zaproszenia w ich społeczności. Kody są nasze — każdy signup jest przypisany i zgodny z zgodą.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź Beacon i liczbę kodów".into(), why_it_matters: "Upewnij się, że partner jest właściwy".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać prośbę".into(), why_it_matters: "Po akceptacji prośba zostanie wysłana do Beacon".into() },
                ],
                content: vec![
                    BriefingField { label: "Beacon".into(), value: beacon_id.to_string() },
                    BriefingField { label: "Wydarzenie".into(), value: event_id.to_string() },
                    BriefingField { label: "Liczba kodów".into(), value: requested_count.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestOutreachDiscovery { requested_candidates } => ActionBriefing {
                summary: format!("Znajdź {} kandydatów outreach", requested_candidates),
                why_it_matters: "System przeszuka opublikowane źródła w poszukiwaniu tras zgłoszeń. Odczyt danych publicznych — nie kontaktuje nikogo.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby uruchomić wyszukiwanie".into(), why_it_matters: "System znajdzie potencjalne cele outreach".into() },
                ],
                content: vec![
                    BriefingField { label: "Liczba kandydatów".into(), value: requested_candidates.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBookingTargetDiscovery { requested_count } => ActionBriefing {
                summary: format!("Znajdź {} celów bookingowych", requested_count),
                why_it_matters: "System przeszuka opublikowane trasy venue/promoter. Odczyt danych publicznych — nie kontaktuje nikogo.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby uruchomić wyszukiwanie".into(), why_it_matters: "System znajdzie potencjalne cele bookingowe".into() },
                ],
                content: vec![
                    BriefingField { label: "Liczba celów".into(), value: requested_count.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestBeaconOutreach { beacon_id, event_id, phase, template_key, .. } => ActionBriefing {
                summary: format!("Kontakt Beacon: {}", template_key),
                why_it_matters: "To podejście do Beacon w sprawie wydarzenia. Otrzymasz jedną szansę na kontakt.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź Beacon, fazę i szablon".into(), why_it_matters: "Upewnij się, że wiadomość jest odpowiednia".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać".into(), why_it_matters: "Po akceptacji wiadomość zostanie wysłana".into() },
                ],
                content: vec![
                    BriefingField { label: "Beacon".into(), value: beacon_id.to_string() },
                    BriefingField { label: "Wydarzenie".into(), value: event_id.to_string() },
                    BriefingField { label: "Faza".into(), value: format!("{:?}", phase) },
                    BriefingField { label: "Szablon".into(), value: template_key.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestShowGrowth { event_id, lever, template_key } => ActionBriefing {
                summary: format!("Wzmocnij frekwencję: {}", lever.as_str()),
                why_it_matters: "To akcja wzmacniająca frekwencję na koncercie. Może kontaktować zewnętrzne strony lub wysyłać do fanów.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź dźwignię i szablon".into(), why_it_matters: "Upewnij się, że akcja jest odpowiednia dla wydarzenia".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby uruchomić".into(), why_it_matters: "Po akceptacji akcja zostanie wykonana".into() },
                ],
                content: vec![
                    BriefingField { label: "Wydarzenie".into(), value: event_id.to_string() },
                    BriefingField { label: "Dźwignia".into(), value: lever.as_str().into() },
                    BriefingField { label: "Szablon".into(), value: template_key.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestContentArtifact { source_id, artifact, template_key, .. } => ActionBriefing {
                summary: format!("Artefakt treści: {}", template_key),
                why_it_matters: "System wygeneruje artefakt treści (np. grafikę, tekst) ze źródła. To operacja wewnętrzna.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wygenerować".into(), why_it_matters: "System utworzy artefakt z określonego źródła".into() },
                ],
                content: vec![
                    BriefingField { label: "Źródło".into(), value: source_id.to_string() },
                    BriefingField { label: "Artefakt".into(), value: format!("{:?}", artifact) },
                    BriefingField { label: "Szablon".into(), value: template_key.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::AdjustExperiment { experiment_id, winner_variant_id, allocations, complete, .. } => ActionBriefing {
                summary: if *complete { "Zakończ eksperyment i ogłoś zwycięzcę".into() } else { "Dostosuj alokację eksperymentu".into() },
                why_it_matters: "Zmiana alokacji wpływa na to, który wariant widzą fani. Zakończenie eksperymentu ustala zwycięzcę.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź zwycięski wariant i alokacje".into(), why_it_matters: "Upewnij się, że decyzja jest oparta na danych".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zastosować".into(), why_it_matters: "Po akceptacji alokacje zostaną zmienione".into() },
                ],
                content: {
                    let mut fields = vec![
                        BriefingField { label: "Eksperyment".into(), value: experiment_id.to_string() },
                        BriefingField { label: "Zwycięski wariant".into(), value: winner_variant_id.to_string() },
                    ];
                    for alloc in allocations {
                        fields.push(BriefingField {
                            label: format!("Wariant {}", alloc.variant_id),
                            value: format!("{}%", alloc.allocation_basis_points / 100),
                        });
                    }
                    fields.push(BriefingField { label: "Zakończ".into(), value: if *complete { "tak" } else { "nie" }.into() });
                    fields
                },
                deadline_note: String::new(),
            },
            Self::CompleteShowTask { event_id, task } => ActionBriefing {
                summary: format!("Zadanie koncertowe: {:?}", task),
                why_it_matters: "To zadanie operacyjne związane z koncertem. Oznaczenie jako ukończone domyka checklistę.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Potwierdź, że zadanie jest wykonane".into(), why_it_matters: "Zaznacz tylko jeśli faktycznie zostało zrobione".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby domknąć".into(), why_it_matters: "Po akceptacji zadanie zostanie oznaczone jako ukończone".into() },
                ],
                content: vec![
                    BriefingField { label: "Wydarzenie".into(), value: event_id.to_string() },
                    BriefingField { label: "Zadanie".into(), value: format!("{:?}", task) },
                ],
                deadline_note: String::new(),
            },
            Self::EscalateShowTask { event_id, task } => ActionBriefing {
                summary: format!("Eskaluj zadanie koncertowe: {:?}", task),
                why_it_matters: "Eskalacja oznacza, że zadanie wymaga pilnej uwagi. To podnosi priorytet w kolejce.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź dlaczego zadanie wymaga eskalacji".into(), why_it_matters: "Zrozum problem przed podjęciem działania".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby eskalować".into(), why_it_matters: "Po akceptacji priorytet zostanie podniesiony".into() },
                ],
                content: vec![
                    BriefingField { label: "Wydarzenie".into(), value: event_id.to_string() },
                    BriefingField { label: "Zadanie".into(), value: format!("{:?}", task) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestPromotionBudgetChange { campaign_id, from_minor, to_minor, roas_basis_points } => ActionBriefing {
                summary: format!("Zmień budżet promocji: {} → {}", format_minor(*from_minor), format_minor(*to_minor)),
                why_it_matters: "Zmiana budżetu wpływa na wydatki reklamowe. ROAS pokazuje zwrot z wydatków.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź nowy budżet i ROAS".into(), why_it_matters: "Upewnij się, że zmiana jest uzasadniona".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zastosować".into(), why_it_matters: "Po akceptacji budżet zostanie zmieniony".into() },
                ],
                content: vec![
                    BriefingField { label: "Kampania".into(), value: campaign_id.to_string() },
                    BriefingField { label: "Stary budżet".into(), value: format_minor(*from_minor) },
                    BriefingField { label: "Nowy budżet".into(), value: format_minor(*to_minor) },
                    BriefingField { label: "ROAS".into(), value: format!("{}%", roas_basis_points / 100) },
                ],
                deadline_note: String::new(),
            },
            Self::ExecuteReleaseMilestone { title, release_at, milestone, .. } => ActionBriefing {
                summary: format!("Kamień milowy release: {}", title),
                why_it_matters: "To kamień milowy w planie release. Wykonanie uruchamia zaplanowane akcje promocyjne.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź tytuł, datę i typ kamienia milowego".into(), why_it_matters: "Upewnij się, że wszystko jest gotowe".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wykonać".into(), why_it_matters: "Po akceptacji kamień milowy zostanie wykonany".into() },
                ],
                content: vec![
                    BriefingField { label: "Tytuł".into(), value: title.clone() },
                    BriefingField { label: "Data release".into(), value: release_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default() },
                    BriefingField { label: "Kamień milowy".into(), value: format!("{:?}", milestone) },
                ],
                deadline_note: String::new(),
            },
            Self::EscalateEditorialPitch { title, due_at, .. } => ActionBriefing {
                summary: format!("Eskaluj pitch editorial: {}", title),
                why_it_matters: "To przypomnienie o niezłożonym pitchu do Spotify Editorial. Nudge wewnątrz workspace — nie kontaktuje zewnętrznych stron.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź tytuł i termin".into(), why_it_matters: "Zrozum co jest zaległe".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby eskalować".into(), why_it_matters: "Po akceptacji przypomnienie zostanie wysłane".into() },
                ],
                content: vec![
                    BriefingField { label: "Tytuł".into(), value: title.clone() },
                    BriefingField { label: "Termin".into(), value: due_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default() },
                ],
                deadline_note: String::new(),
            },
            Self::ApplyLiveOpportunity { opportunity_id, opportunity_kind, score } => ActionBriefing {
                summary: format!("Wyślij zgłoszenie koncertowe: {:?}", opportunity_kind),
                why_it_matters: "To zgłoszenie na koncert/festiwal. Wysłanie zgłoszenia to zobowiązanie kalendarzowe.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź typ zgłoszenia i wynik".into(), why_it_matters: "Upewnij się, że to właściwa okazja".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać zgłoszenie".into(), why_it_matters: "Po akceptacji zgłoszenie zostanie wysłane".into() },
                ],
                content: vec![
                    BriefingField { label: "Okazja".into(), value: opportunity_id.to_string() },
                    BriefingField { label: "Typ".into(), value: format!("{:?}", opportunity_kind) },
                    BriefingField { label: "Wynik".into(), value: score.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::CounterLiveOpportunityTerms { opportunity_id, ask_minor, currency, round } => ActionBriefing {
                summary: format!("Kontruj warunki: {} {} (runda {})", format_minor(*ask_minor), currency, round),
                why_it_matters: "To kontrpropozycja fee dla promotora. Wysłanie zmienia warunki negocjacji.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź kwotę i walutę".into(), why_it_matters: "Upewnij się, że kwota jest akceptowalna".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać kontrpropozycję".into(), why_it_matters: "Po akceptacji kontrpropozycja zostanie wysłana".into() },
                ],
                content: vec![
                    BriefingField { label: "Okazja".into(), value: opportunity_id.to_string() },
                    BriefingField { label: "Kwota".into(), value: format_minor(*ask_minor) },
                    BriefingField { label: "Waluta".into(), value: currency.clone() },
                    BriefingField { label: "Runda".into(), value: round.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::AcceptLiveOpportunityTerms { opportunity_id, fee_minor, currency } => ActionBriefing {
                summary: format!("Akceptuj warunki: {} {}", format_minor(*fee_minor), currency),
                why_it_matters: "Akceptacja fee to zobowiązanie kalendarza i pieniędzy. Nie można cofnąć po akceptacji.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź kwotę i walutę".into(), why_it_matters: "To zobowiązanie — upewnij się, że warunki są dobre".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zaakceptować".into(), why_it_matters: "Po akceptacji warunki są wiążące".into() },
                ],
                content: vec![
                    BriefingField { label: "Okazja".into(), value: opportunity_id.to_string() },
                    BriefingField { label: "Fee".into(), value: format_minor(*fee_minor) },
                    BriefingField { label: "Waluta".into(), value: currency.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::PrepareFundingPackage { opportunity_id } => ActionBriefing {
                summary: "Przygotuj pakiet finansowania".into(),
                why_it_matters: "To przygotowanie dokumentów wniosku finansowego. Operacja wewnętrzna — nie kontaktuje zewnętrznych stron.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby przygotować pakiet".into(), why_it_matters: "System zbierze wymagane dokumenty".into() },
                ],
                content: vec![
                    BriefingField { label: "Okazja".into(), value: opportunity_id.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::SubmitFundingApplication { opportunity_id } => ActionBriefing {
                summary: "Wyślij wniosek finansowy".into(),
                why_it_matters: "Wysłanie wniosku to formalne zobowiązanie. Po wysłaniu nie można cofnąć.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź kompletność wniosku".into(), why_it_matters: "Upewnij się, że wszystkie dokumenty są gotowe".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać".into(), why_it_matters: "Po akceptacji wniosek zostanie wysłany".into() },
                ],
                content: vec![
                    BriefingField { label: "Okazja".into(), value: opportunity_id.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RaiseGrowthOpportunity { platform, metric_key, signal, recommended_action, deviation_basis_points, .. } => ActionBriefing {
                summary: format!("Możliwość wzrostu: {} — {}", platform_label(platform), metric_key),
                why_it_matters: "System wykrył ruch w zewnętrznej metryce. To sygnał, że coś się dzieje i wymaga reakcji.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Przeczytaj zalecaną akcję".into(), why_it_matters: "Zrozum co system proponuje i dlaczego".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zaplanować akcję".into(), why_it_matters: "Po akceptacji akcja zostanie dodana do kolejki".into() },
                ],
                content: vec![
                    BriefingField { label: "Platforma".into(), value: platform_label(platform) },
                    BriefingField { label: "Metryka".into(), value: metric_key.clone() },
                    BriefingField { label: "Sygnał".into(), value: format!("{:?}", signal) },
                    BriefingField { label: "Odchylenie".into(), value: format!("{}%", deviation_basis_points / 100) },
                    BriefingField { label: "Zalecana akcja".into(), value: recommended_action.clone() },
                ],
                deadline_note: String::new(),
            },
            Self::IssueReferralCode { fan_id } => ActionBriefing {
                summary: "Wydaj kod referencyjny fanowi".into(),
                why_it_matters: "Kod referencyjny to mechanizm wzrostu, który skaluje się z audytorium. Fan musi wyrazić zgodę.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wydać kod".into(), why_it_matters: "Po akceptacji fan otrzyma swój kod referencyjny".into() },
                ],
                content: vec![
                    BriefingField { label: "Fan".into(), value: fan_id.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RaiseGrowthDebt { debt_kind, recommended_action, overdue_basis_points, outstanding_items, tracked_items, .. } => ActionBriefing {
                summary: format!("Dług wzrostu: {:?}", debt_kind),
                why_it_matters: "To zaległa praca, która została zobowiązana ale nie wykonana. Im dłużej czeka, tym trudniej ją nadrobić.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Przeczytaj zalecaną akcję".into(), why_it_matters: "Zrozum co jest zaległe i dlaczego".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby zaplanować nadrobienie".into(), why_it_matters: "Po akceptacji akcja zostanie dodana do kolejki".into() },
                ],
                content: vec![
                    BriefingField { label: "Typ długu".into(), value: format!("{:?}", debt_kind) },
                    BriefingField { label: "Zalecana akcja".into(), value: recommended_action.clone() },
                    BriefingField { label: "Po terminie".into(), value: format!("{}%", overdue_basis_points / 100) },
                    BriefingField { label: "Zaległe pozycje".into(), value: format!("{} / {}", outstanding_items, tracked_items) },
                ],
                deadline_note: String::new(),
            },
            Self::RunPlayStep { play_id, play_kind, step_index, step_kind, event_id, fan_id, template_key } => ActionBriefing {
                summary: format!("Krok play: {:?} (krok {})", play_kind, step_index),
                why_it_matters: "To jeden krok kampanii play dla jednego fana. Wysłanej wiadomości nie można cofnąć.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź typ kroku i szablon".into(), why_it_matters: "Upewnij się, że treść jest odpowiednia dla tego kroku".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać".into(), why_it_matters: "Po akceptacji krok zostanie wykonany".into() },
                ],
                content: {
                    let mut fields = vec![
                        BriefingField { label: "Play".into(), value: play_id.to_string() },
                        BriefingField { label: "Typ play".into(), value: format!("{:?}", play_kind) },
                        BriefingField { label: "Krok".into(), value: format!("{}: {:?}", step_index, step_kind) },
                        BriefingField { label: "Szablon".into(), value: template_key.clone() },
                    ];
                    if let Some(eid) = event_id {
                        fields.push(BriefingField { label: "Wydarzenie".into(), value: eid.to_string() });
                    }
                    if let Some(fid) = fan_id {
                        fields.push(BriefingField { label: "Fan".into(), value: fid.to_string() });
                    }
                    fields
                },
                deadline_note: String::new(),
            },
            Self::SendTeamAssignmentEmail { task_title, task_detail, reminder_number, .. } => ActionBriefing {
                summary: if *reminder_number > 0 { format!("Przypomnienie: {}", task_title) } else { task_title.clone() },
                why_it_matters: "To email z przypisaniem zadania do członka zespołu. Przypomnienia są wysyłane aż zadanie zostanie domknięte.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź treść zadania".into(), why_it_matters: "Upewnij się, że zadanie jest jasne".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać email".into(), why_it_matters: "Po akceptacji email zostanie wysłany".into() },
                ],
                content: vec![
                    BriefingField { label: "Tytuł zadania".into(), value: task_title.clone() },
                    BriefingField { label: "Szczegóły".into(), value: truncate(task_detail.clone(), 2000) },
                    BriefingField { label: "Przypomnienie".into(), value: reminder_number.to_string() },
                ],
                deadline_note: String::new(),
            },
            Self::RequestAgentContent { template_id, task_id, draft } => ActionBriefing {
                summary: "Zatwierdź draft treści od agenta".into(),
                why_it_matters: "Agent wygenerował ten draft na podstawie inteligencji zebranej przez system. Po akceptacji treść zostanie opublikowana w odpowiednim kanale.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Przeczytaj draft poniżej".into(), why_it_matters: "Sprawdź ton, fakty i zgodność z marką".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ jeśli draft jest dobry, ODRZUĆ jeśli wymaga poprawek".into(), why_it_matters: "Po akceptacji treść zostanie opublikowana — nie można cofnąć".into() },
                ],
                content: {
                    let mut fields = Vec::new();
                    if let Some(tid) = template_id {
                        fields.push(BriefingField { label: "Szablon".into(), value: tid.clone() });
                    }
                    fields.push(BriefingField { label: "Zadanie".into(), value: task_id.to_string() });
                    fields.push(BriefingField { label: "Draft".into(), value: truncate(draft_to_text(draft), 2000) });
                    fields
                },
                deadline_note: String::new(),
            },
            Self::RequestAgentRun { template_id, prompt, priority, tier } => ActionBriefing {
                summary: format!("Uruchom agenta: {}", template_id),
                why_it_matters: "Mózg (deterministyczny) wysyła pracownika LLM aby zebrał inteligencję lub wygenerował draft. Agent nie podejmuje decyzji — tylko dostarcza dane.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź szablon i priorytet".into(), why_it_matters: "Upewnij się, że zadanie ma sens".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby uruchomić agenta".into(), why_it_matters: "Po akceptacji agent zostanie uruchomiony i zbierze inteligencję".into() },
                ],
                content: vec![
                    BriefingField { label: "Szablon".into(), value: template_id.clone() },
                    BriefingField { label: "Priorytet".into(), value: priority.to_string() },
                    BriefingField { label: "Tier".into(), value: format!("{:?}", tier) },
                    BriefingField { label: "Prompt".into(), value: truncate(prompt.clone(), 2000) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestCommunityEngagement { platform, subreddit, title, body, smart_link, .. } => ActionBriefing {
                summary: format!("Post społecznościowy: {} — {}", platform, title),
                why_it_matters: "To post na zewnętrznej platformie (np. Reddit). Po opublikowaniu nie można go cofnąć. Post trafi do cudzej społeczności.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Przeczytaj tytuł i treść posta".into(), why_it_matters: "Sprawdź ton, fakty i zgodność z zasadami platformy".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby opublikować".into(), why_it_matters: "Po akceptacji post zostanie opublikowany na platformie".into() },
                ],
                content: vec![
                    BriefingField { label: "Platforma".into(), value: platform.clone() },
                    BriefingField { label: "Subreddit".into(), value: subreddit.clone().unwrap_or("—".into()) },
                    BriefingField { label: "Tytuł".into(), value: title.clone() },
                    BriefingField { label: "Treść".into(), value: truncate(body.clone(), 2000) },
                    BriefingField { label: "Smart link".into(), value: smart_link.clone().unwrap_or("—".into()) },
                ],
                deadline_note: String::new(),
            },
            Self::RequestSignalPush { title, body, target_path, event_id, segment, .. } => ActionBriefing {
                summary: format!("Powiadomienie push: {}", title),
                why_it_matters: "Powiadomienie push trafi do fanów, którzy wyrazili zgodę na powiadomienia. Wysłany push nie może być cofnięty.".into(),
                steps: vec![
                    BriefingStep { what_to_do: "Sprawdź tytuł i treść powiadomienia".into(), why_it_matters: "Wysłany push nie może być cofnięty — sprawdź dokładnie".into() },
                    BriefingStep { what_to_do: "Kliknij AKCEPTUJ aby wysłać do segmentu".into(), why_it_matters: "Po akceptacji push zostanie wysłany do wybranego segmentu fanów".into() },
                ],
                content: vec![
                    BriefingField { label: "Tytuł".into(), value: title.clone() },
                    BriefingField { label: "Treść".into(), value: truncate(body.clone(), 2000) },
                    BriefingField { label: "Link".into(), value: target_path.clone().unwrap_or("—".into()) },
                    BriefingField { label: "Segment".into(), value: segment.clone().unwrap_or("wszyscy".into()) },
                    BriefingField { label: "Wydarzenie".into(), value: event_id.map(|id| id.to_string()).unwrap_or("—".into()) },
                ],
                deadline_note: String::new(),
            },
        }
    }
}

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
    }
}

/// Extracts readable text from a draft JSON value.
/// Drafts are structured JSON from the agent service; this flattens them
/// into a single string for display. Falls back to pretty-printed JSON.
fn draft_to_text(draft: &serde_json::Value) -> String {
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
}
