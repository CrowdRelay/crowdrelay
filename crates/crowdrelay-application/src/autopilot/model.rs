//! Stable application-boundary types for ViryaOS Autopilot.

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
    funding::FundingPolicy,
    growth_debt::{GrowthDebtKind, GrowthDebtPolicy, GrowthDebtSubject},
    growth_metrics::{GrowthMetricPolicy, GrowthSignal, MetricDirection, MetricPlatform},
    learning::{PlayRecord, PlayStanding},
    live_opportunities::{LiveOpportunityKind, LiveOpportunityPolicy},
    merch_bundle::MerchBundlePolicy,
    merchandising::{MerchPricePolicy, MerchReorderPolicy},
    outreach::{OutreachPhase, OutreachPolicy},
    play_measurement::PlayClaim,
    plays::{PlayKind, PlayPolicy, PlayStepKind, PlayStepState, StepSkipReason},
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
    pub const ALL: [Self; 21] = [
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
            Self::Plays => "plays",
        }
    }
}

/// Typed bounded-context configuration loaded from the policy store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    Plays(PlayPolicy),
}

/// Persisted authority configuration for one bounded context.
#[derive(Clone, Debug, Eq, PartialEq)]
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
    },
    RequestBeaconDiscovery {
        event_id: EventId,
        target_count: u16,
    },
    /// Ask an adapter to sweep published sources for submission routes and post
    /// the candidates back. Reads public data, contacts nobody, and buys
    /// nothing; the screening that decides what is admissible stays here.
    RequestOutreachDiscovery {
        requested_candidates: u16,
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
    ApplyLiveOpportunity {
        opportunity_id: TeamOpportunityId,
        opportunity_kind: LiveOpportunityKind,
        score: u16,
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
        /// The show the step is anchored to. Carried so the executor renders
        /// the ask about a specific date rather than about the band in general.
        event_id: EventId,
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
            | Self::ApplyLiveOpportunity { .. }
            | Self::SubmitFundingApplication { .. } => ActionClass::ThirdParty,

            // Fans who opted in. Free, but a sent message cannot be unsent.
            Self::RequestFanLifecycleMessage { .. } | Self::RequestAudienceCampaign { .. } => {
                ActionClass::OwnedAudience
            }

            // Ours, free and undoable by doing the opposite. The team
            // assignment email is here deliberately: it reaches our own staff,
            // not an audience or a stranger, and treating internal task routing
            // as outward contact would spend the audience budget on ourselves.
            Self::RequestBeaconDiscovery { .. }
            | Self::RequestOutreachDiscovery { .. }
            | Self::RequestContentArtifact { .. }
            | Self::AdjustExperiment { .. }
            | Self::CompleteShowTask { .. }
            | Self::EscalateShowTask { .. }
            | Self::PrepareFundingPackage { .. }
            | Self::RaiseGrowthOpportunity { .. }
            | Self::RaiseGrowthDebt { .. }
            | Self::IssueReferralCode { .. }
            | Self::SendTeamAssignmentEmail { .. } => ActionClass::FirstPartyReversible,

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
                ReleaseMilestone::SeedCalendar | ReleaseMilestone::Wrap => {
                    ActionClass::FirstPartyReversible
                }
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
            Self::PrepareFundingPackage { .. } => "funding.package.prepare",
            Self::SubmitFundingApplication { .. } => "funding.application.submit",
            Self::RaiseGrowthOpportunity { .. } => "growth.opportunity.raise",
            Self::RaiseGrowthDebt { .. } => "growth.debt.raise",
            Self::IssueReferralCode { .. } => "referral.code.issue",
            Self::RunPlayStep { .. } => "play.step.run",
            Self::SendTeamAssignmentEmail { .. } => "team.assignment.email",
        }
    }
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
}

/// One kind of play, its measured record and what that record is allowed to
/// change about it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PlayKindStanding {
    pub kind: PlayKind,
    pub record: PlayRecord,
    pub standing: PlayStanding,
    /// The operator's ceiling narrowed by the record. Never widened: a perfect
    /// record still reaches exactly the number an operator configured.
    pub effective_max_recipients_per_step: u32,
}

/// A fact the agent could hang a campaign on, before any play exists for it.
///
/// Read separately from running plays because there is no state machine yet:
/// this is an anchor being considered, not a play being advanced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayAnchor {
    pub event_id: EventId,
    pub anchor_at: OffsetDateTime,
    /// False for a show that is cancelled or no longer published. Carried
    /// rather than filtered away in SQL so the refusal to start is a domain
    /// rule somebody can read, not a `WHERE` clause somebody can loosen.
    pub active: bool,
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
    pub event_id: EventId,
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
    pub event_id: EventId,
    pub anchor_at: OffsetDateTime,
    pub anchor_active: bool,
    pub steps: Vec<PlayStepState>,
    pub audience: PlayAudience,
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
