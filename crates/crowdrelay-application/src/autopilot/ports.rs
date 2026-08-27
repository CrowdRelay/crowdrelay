//! Infrastructure ports used by Autopilot evaluation, execution and measurement.

use async_trait::async_trait;
use crowdrelay_domain::{
    AutopilotActionId, AutopilotMeasurementId, PlayId, WorkspaceId,
    action_class::ActionClass,
    audience_lifecycle::FanLifecycleSnapshot,
    autonomy::AutonomyLevel,
    beacons::{BeaconCampaignSnapshot, BeaconDiscoverySnapshot, BeaconInviteSnapshot},
    booking::{BookingTargetSnapshot, CityOpportunitySnapshot},
    campaign_lifecycle::EventCampaignSnapshot,
    content_supply::ContentSupplySnapshot,
    experimentation::ExperimentSnapshot,
    funding::FundingOpportunitySnapshot,
    growth_debt::GrowthDebtObservation,
    growth_envelope::{EnvelopeUsage, GrowthEnvelope},
    growth_metrics::GrowthMetricSnapshot,
    learning::{WaveOutcomeVerdict, WaveReplyCounts, assess_wave_outcome},
    live_opportunities::LiveOpportunitySnapshot,
    merch_bundle::MerchBundleSnapshot,
    merchandising::{MerchInventorySnapshot, MerchPriceSnapshot},
    outreach::{OutreachSnapshot, OutreachTargetKind},
    performance::{EffectDirection, EffectResult, assess_effect},
    play_measurement::{
        PlayMeasurementPolicy, PlayOutcomeInput, PlayOutcomeVerdict, assess_play_outcome,
        window_velocity_milli_per_day,
    },
    plays::{PlayKind, PlayPolicy},
    pricing::TicketYieldSnapshot,
    promotion::PromotionPerformanceSnapshot,
    release_autopilot::ReleasePlanSnapshot,
    show_growth::ShowGrowthSnapshot,
    show_operations::ShowTaskSnapshot,
    target_discovery::OutreachSupplySnapshot,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crowdrelay_domain::deliverability::DeliverabilitySnapshot;

use super::model::{
    AutopilotPolicy, CandidatePersistence, ClaimedAutopilotAction, ClaimedPlayOutcome,
    DecisionCandidate, LiveTermsSnapshot, OutreachKindStanding, OutreachWaveAnchor,
    OutreachWaveSnapshot, OutreachWaveStart, OutreachWaveTransition, PlacementSettlement,
    PlayAnchor, PlayKindStanding, PlayOutcomeObservation, PlayRunSnapshot, PlayStart,
    PlayStepSettlement, PlaylistPlacementSnapshot, TermsSettlement,
};
use crate::RepositoryError;

#[async_trait]
pub trait AutopilotDecisionRepository: Send + Sync {
    async fn load_policies(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<AutopilotPolicy>, RepositoryError>;

    async fn load_ticket_yield_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<TicketYieldSnapshot>, RepositoryError>;

    async fn load_fan_lifecycle_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FanLifecycleSnapshot>, RepositoryError>;

    async fn load_event_campaign_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<EventCampaignSnapshot>, RepositoryError>;

    async fn load_merch_inventory_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchInventorySnapshot>, RepositoryError>;

    async fn load_merch_price_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchPriceSnapshot>, RepositoryError>;

    async fn load_merch_bundle_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchBundleSnapshot>, RepositoryError>;

    async fn load_city_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<CityOpportunitySnapshot>, RepositoryError>;

    async fn load_booking_target_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BookingTargetSnapshot>, RepositoryError>;

    async fn load_outreach_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachSnapshot>, RepositoryError>;

    async fn load_content_supply_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ContentSupplySnapshot>, RepositoryError>;

    async fn load_experiment_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ExperimentSnapshot>, RepositoryError>;

    async fn load_show_task_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowTaskSnapshot>, RepositoryError>;

    async fn load_promotion_performance_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PromotionPerformanceSnapshot>, RepositoryError>;

    async fn load_release_plan_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ReleasePlanSnapshot>, RepositoryError>;

    async fn load_live_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveOpportunitySnapshot>, RepositoryError>;

    /// What this workspace's sending looks like from outside: how much it has
    /// sent, how much of that bounced or was reported, and when it started.
    async fn load_deliverability_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<DeliverabilitySnapshot, RepositoryError>;

    /// Free-reach waves still being drafted or waiting on a human.
    async fn load_outreach_waves(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachWaveSnapshot>, RepositoryError>;

    /// Anchors — releases and shows — with no wave of a given kind yet.
    async fn load_outreach_wave_anchors(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachWaveAnchor>, RepositoryError>;

    /// Opens a wave. False when another cycle got there first, which is not a
    /// failure: the unique constraint is what makes one wave per anchor true.
    async fn open_outreach_wave(
        &self,
        workspace_id: WorkspaceId,
        start: &OutreachWaveStart,
    ) -> Result<bool, RepositoryError>;

    /// Seals a wave for review, or ends it without approval.
    async fn transition_outreach_wave(
        &self,
        workspace_id: WorkspaceId,
        wave_id: uuid::Uuid,
        transition: OutreachWaveTransition,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// Claimed placements that are not settled yet.
    async fn load_playlist_placements(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PlaylistPlacementSnapshot>, RepositoryError>;

    /// Ends a placement. A withdrawal also suppresses the curator behind it and
    /// every other target sharing their identity, in the same transaction:
    /// suppressing the playlist and leaving the operator pitchable is how the
    /// same person is approached again next week through a different list.
    async fn settle_playlist_placement(
        &self,
        workspace_id: WorkspaceId,
        settlement: PlacementSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// Live negotiations, each with the show it is about.
    async fn load_live_opportunity_terms(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveTermsSnapshot>, RepositoryError>;

    /// Ends a negotiation without an acceptance. Idempotent on the state
    /// already being unsettled, so two cycles racing on the same expired window
    /// leave one recorded reason.
    async fn settle_live_opportunity_terms(
        &self,
        workspace_id: WorkspaceId,
        settlement: &TermsSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn load_funding_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FundingOpportunitySnapshot>, RepositoryError>;

    async fn load_beacon_discovery_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconDiscoverySnapshot>, RepositoryError>;

    async fn load_beacon_campaign_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconCampaignSnapshot>, RepositoryError>;

    /// How many bookable targets the pipeline can contact, and when the agent
    /// last asked for more supply. The booking analogue of outreach supply.
    async fn load_booking_supply_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<crowdrelay_domain::booking_discovery::BookingSupplySnapshot, RepositoryError>;

    /// Verified scene nodes with an upcoming show in their own city, and how
    /// long since — or whether — they were last asked to run invite codes.
    async fn load_beacon_invite_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconInviteSnapshot>, RepositoryError>;

    async fn load_show_growth_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowGrowthSnapshot>, RepositoryError>;

    /// Returns every active metric series with its derived trend and the two
    /// pieces of context the rule needs but cannot see from one series alone:
    /// how long ago this series last produced a decision, and whether the same
    /// platform has a stronger-tier series being tracked.
    async fn load_growth_metric_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthMetricSnapshot>, RepositoryError>;

    /// Returns one observation per subject that has outstanding committed work,
    /// across every debt kind, already carrying `hours_since_last_signal`.
    ///
    /// The adapter reports facts only — how long the work has been outstanding,
    /// how much of it is outstanding, and what date applies. Every horizon and
    /// threshold lives in `GrowthDebtPolicy`, so what counts as neglect can
    /// change without touching a query.
    async fn load_growth_debt_observations(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthDebtObservation>, RepositoryError>;

    /// What the pitcher currently has to work with. One row per workspace
    /// rather than a list: supply is not a property of any single target, and
    /// counting it per target is how a starved pipeline stays invisible.
    async fn load_outreach_supply_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<OutreachSupplySnapshot, RepositoryError>;

    /// Anchors that could carry a play of this kind and do not have one yet.
    ///
    /// Scoped by kind because "already has a play" is per kind: a show may run
    /// a track-us play and a listing sweep, and one query that ignored the kind
    /// would start the second only until the first existed.
    async fn load_play_anchors(
        &self,
        workspace_id: WorkspaceId,
        kind: PlayKind,
        now: OffsetDateTime,
    ) -> Result<Vec<PlayAnchor>, RepositoryError>;

    /// What the measured record says about each kind of play.
    ///
    /// Read once per cycle rather than per anchor: a standing is a property of
    /// the play kind, and re-reading it for every show would be a query per
    /// candidate for an answer that cannot change mid-cycle.
    async fn load_play_standings(
        &self,
        workspace_id: WorkspaceId,
        policy: PlayPolicy,
    ) -> Result<Vec<PlayKindStanding>, RepositoryError>;

    /// The outreach kind standings, with the operator's wave-size ceiling.
    /// Read once per cycle: a standing is a property of the target kind, and
    /// re-reading it for every wave would be a query per kind for an answer
    /// that cannot change mid-cycle.
    async fn load_outreach_kind_standings(
        &self,
        workspace_id: WorkspaceId,
        max_pitches_per_wave: u32,
    ) -> Result<Vec<OutreachKindStanding>, RepositoryError>;

    /// Creates the play and its whole step schedule in one transaction.
    ///
    /// Returns false when a play already covered this anchor. Not an error: two
    /// cycles racing, or a restart mid-cycle, must leave one play rather than a
    /// failure somebody has to interpret.
    async fn start_play(
        &self,
        workspace_id: WorkspaceId,
        start: &PlayStart,
    ) -> Result<bool, RepositoryError>;

    /// Every running play with the state its next decision needs, including who
    /// its open step has not yet reached.
    async fn load_play_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PlayRunSnapshot>, RepositoryError>;

    /// Settles a step that will never be delivered, with its reason.
    ///
    /// The write that makes an omission a fact. Without it a step nobody
    /// approved simply stops being mentioned, which is the failure mode the
    /// whole design exists to avoid.
    async fn settle_play_step(
        &self,
        workspace_id: WorkspaceId,
        settlement: &PlayStepSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// Marks a play whose every step is settled as complete.
    async fn complete_play(
        &self,
        workspace_id: WorkspaceId,
        play_id: PlayId,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// The operator's autonomy ceiling per action class.
    ///
    /// A class missing from the returned map is read as its safest ceiling, not
    /// as an absent limit: a migration that has not run must never be a grant
    /// of authority.
    async fn load_autonomy_ceilings(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(ActionClass, AutonomyLevel)>, RepositoryError>;

    /// The operator's volume limits, and what the workspace has already spent
    /// against them in the trailing seven days.
    ///
    /// Returned together because they are read together once per cycle: the
    /// limits without the spend cannot decide anything, and reading the spend
    /// per candidate would be a query per finding.
    async fn load_growth_envelope(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<(GrowthEnvelope, EnvelopeUsage), RepositoryError>;

    /// Hours since the agent last reached each subject through an outward
    /// action, for the cooldown. One query per cycle, not one per candidate.
    async fn load_outward_touch_ages(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<(uuid::Uuid, u32)>, RepositoryError>;

    /// Persists the decision and, for executable dispositions, creates exactly
    /// one durable action unless an equivalent action is already in flight.
    async fn persist_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
    ) -> Result<CandidatePersistence, RepositoryError>;
}

/// Durable execution port. Kept separate from decision snapshot access so the
/// evaluator cannot accidentally grow side-effect responsibilities.
#[async_trait]
pub trait AutopilotActionRepository: Send + Sync {
    async fn claim_due_actions(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError>;

    async fn execute_action(
        &self,
        workspace_id: WorkspaceId,
        action: &ClaimedAutopilotAction,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

/// Settling what a play did, one claim at a time.
///
/// Kept apart from [`AutopilotMeasurementRepository`] because the two measure
/// different things and must not be confused. That one measures an *action*
/// against a metric it moved directly. This one measures a *play* — a campaign
/// of many sends — and its answers are claims with a named strength, including
/// the answer "this cannot be known, and here is why".
#[async_trait]
pub trait AutopilotPlayOutcomeRepository: Send + Sync {
    async fn claim_due_play_outcomes(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedPlayOutcome>, RepositoryError>;

    /// Reads the window. Never writes, and never fills a gap: a missing series,
    /// an ambiguous one and an absent join key all come back as themselves.
    async fn observe_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedPlayOutcome,
        now: OffsetDateTime,
    ) -> Result<PlayOutcomeObservation, RepositoryError>;

    async fn complete_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedPlayOutcome,
        observation: &PlayOutcomeObservation,
        verdict: PlayOutcomeVerdict,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome_id: uuid::Uuid,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

/// A wave outcome that is due for settlement.
#[derive(Clone, Debug)]
pub struct ClaimedWaveOutcome {
    pub id: uuid::Uuid,
    pub wave_id: uuid::Uuid,
    pub target_kind: OutreachTargetKind,
    pub pitches_sent: u32,
    pub window_start: OffsetDateTime,
    pub window_end: OffsetDateTime,
    pub attempt_number: u32,
}

/// The reply counts read from the interaction table for one wave.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaveOutcomeObservation {
    pub positive_replies: u32,
    pub declined_replies: u32,
    pub do_not_contact_replies: u32,
    pub total_replies: u32,
    pub observed_at: OffsetDateTime,
}

/// Settling what a wave did, one wave at a time.
///
/// Kept apart from [`AutopilotPlayOutcomeRepository`] because a wave measures
/// replies directly — no metric series, no baseline, no trend — and confusing
/// the two would make a wave read a play's series or vice versa.
#[async_trait]
pub trait AutopilotWaveOutcomeRepository: Send + Sync {
    /// Claims due wave outcomes for settlement. Same `FOR UPDATE SKIP LOCKED`
    /// pattern as play outcomes: two workers never settle the same wave.
    async fn claim_due_wave_outcomes(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedWaveOutcome>, RepositoryError>;

    /// Reads the reply counts for the wave's targets in the window. Never
    /// writes, and never fills a gap: a wave with no replies is `NoReplies`,
    /// not zero-against-a-baseline.
    async fn observe_wave_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedWaveOutcome,
        now: OffsetDateTime,
    ) -> Result<WaveOutcomeObservation, RepositoryError>;

    /// Completes the outcome and folds the verdict into the per-kind learning
    /// record, in one transaction.
    async fn complete_wave_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedWaveOutcome,
        observation: &WaveOutcomeObservation,
        verdict: WaveOutcomeVerdict,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_wave_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome_id: uuid::Uuid,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

/// Turns one wave observation into the verdict that will be stored.
///
/// A free function rather than a method so the rule stays testable without a
/// database, and so the worker cannot reach a different answer than the one the
/// domain would give.
#[must_use]
pub fn assess_wave_claim(
    outcome: &ClaimedWaveOutcome,
    observation: &WaveOutcomeObservation,
) -> WaveOutcomeVerdict {
    assess_wave_outcome(
        WaveReplyCounts {
            positive: observation.positive_replies,
            declined: observation.declined_replies,
            do_not_contact: observation.do_not_contact_replies,
            total: observation.total_replies,
        },
        outcome.pitches_sent,
    )
}

// ---------------------------------------------------------------------------
// Reply triage — first-party classification of inbound replies.
//
// n8n posts replies with a disposition it assigned. When the disposition is
// `Received` (unclassified), the worker re-classifies using the domain
// classifier and records the result. Replies that need human review are
// surfaced via the operator brief.
// ---------------------------------------------------------------------------

/// A reply awaiting first-party classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyNeedingTriage {
    pub reply_id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub target_kind: OutreachTargetKind,
    pub reply_text: String,
    pub previous_disposition: Option<crowdrelay_domain::outreach::OutreachReplyDisposition>,
}

/// The result of classifying one reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyTriageResult {
    pub classification: crowdrelay_domain::reply_triage::ReplyClassification,
    pub classified_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotReplyTriageRepository: Send + Sync {
    /// Loads replies with `Received` disposition that have not been classified
    /// by the first-party classifier yet. Bounded by `limit`.
    async fn load_replies_needing_triage(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<ReplyNeedingTriage>, RepositoryError>;

    /// Records the classification for a reply and updates the reply's
    /// disposition if the classifier produced an auto-classification.
    /// For `NeedsHuman`, the disposition stays `Received` and the
    /// classification is stored for the operator brief to surface.
    async fn record_reply_classification(
        &self,
        workspace_id: WorkspaceId,
        reply_id: uuid::Uuid,
        result: &ReplyTriageResult,
    ) -> Result<(), RepositoryError>;
}
///
/// A free function rather than a method so the rule stays testable without a
/// database, and so the worker cannot reach a different answer than the one the
/// domain would give.
pub fn assess_play_claim(
    outcome: &ClaimedPlayOutcome,
    observation: &PlayOutcomeObservation,
    policy: PlayMeasurementPolicy,
) -> PlayOutcomeVerdict {
    assess_play_outcome(
        PlayOutcomeInput {
            claim: outcome.claim,
            recipients_reached: observation.recipients_reached,
            baseline_milli_per_day: outcome.baseline_milli_per_day,
            window_milli_per_day: observation.observed_value.and_then(|observed| {
                outcome.baseline_value.and_then(|baseline| {
                    window_velocity_milli_per_day(
                        baseline,
                        observed,
                        observation.observed_at - outcome.window_start,
                    )
                })
            }),
            attributed_clicks: observation.attributed_clicks,
            direction: observation.direction,
            ambiguous_series: observation.ambiguous_series,
        },
        policy,
    )
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotMeasurementKind {
    TicketRevenue72h,
    MerchGrossProxy7d,
    PromotionRoas7d,
    BookingReply7d,
    OutreachReply7d,
    AudienceTicketRevenue72h,
    ShowTicketRevenue7d,
    ShowGrowthSurfaceClicks7d,
    ShowGrowthAttributedTicketOrders7d,
    GrassrootsActivationReplies14d,
}

impl AutopilotMeasurementKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TicketRevenue72h => "ticket_revenue_72h",
            Self::MerchGrossProxy7d => "merch_gross_proxy_7d",
            Self::PromotionRoas7d => "promotion_roas_7d",
            Self::BookingReply7d => "booking_reply_7d",
            Self::OutreachReply7d => "outreach_reply_7d",
            Self::AudienceTicketRevenue72h => "audience_ticket_revenue_72h",
            Self::ShowTicketRevenue7d => "show_ticket_revenue_7d",
            Self::ShowGrowthSurfaceClicks7d => "show_growth_surface_clicks_7d",
            Self::ShowGrowthAttributedTicketOrders7d => "show_growth_attributed_ticket_orders_7d",
            Self::GrassrootsActivationReplies14d => "grassroots_activation_replies_14d",
        }
    }

    #[must_use]
    pub const fn direction(self) -> EffectDirection {
        EffectDirection::HigherIsBetter
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ClaimedAutopilotMeasurement {
    pub id: AutopilotMeasurementId,
    pub action_id: AutopilotActionId,
    pub kind: AutopilotMeasurementKind,
    pub subject_id: uuid::Uuid,
    pub baseline_value: f64,
    pub action_finished_at: OffsetDateTime,
    pub attempt_number: u32,
}

#[async_trait]
pub trait AutopilotMeasurementRepository: Send + Sync {
    async fn claim_due_measurements(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotMeasurement>, RepositoryError>;

    async fn observe_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<f64, RepositoryError>;

    async fn complete_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observed_value: f64,
        effect: EffectResult,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement_id: AutopilotMeasurementId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

#[must_use]
pub fn assess_measurement_effect(
    measurement: &ClaimedAutopilotMeasurement,
    observed_value: f64,
) -> Option<EffectResult> {
    assess_effect(
        measurement.baseline_value,
        observed_value,
        measurement.kind.direction(),
        500,
    )
}

/// Venue/promoter discovery: the booking pipeline's supply, screened on write.
#[async_trait]
pub trait AutopilotBookingDiscoveryRepository: Send + Sync {
    async fn ingest_booking_candidates(
        &self,
        workspace_id: WorkspaceId,
        candidates: Vec<crowdrelay_domain::booking_discovery::BookingCandidateInput>,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<BookingCandidateIngestion, RepositoryError>;

    /// Promotes one admitted email-route candidate into a real booking target.
    /// The human confirmation is what turns a published route into somebody
    /// the agent may approach.
    async fn confirm_booking_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate_id: crowdrelay_domain::OutreachOpportunityId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<crate::autopilot::AutopilotControlMutation, RepositoryError>;

    async fn list_booking_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<String>,
        limit: u32,
    ) -> Result<Vec<BookingCandidateView>, RepositoryError>;
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BookingCandidateIngestion {
    pub reported: u32,
    pub admitted: u32,
    pub refused: u32,
    /// Found through a second source: contact identity dedupes, so the same
    /// inbox is never two prospects.
    pub duplicates: u32,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BookingCandidateView {
    pub candidate_id: uuid::Uuid,
    pub target_kind: String,
    pub display_name: String,
    pub city_slug: Option<String>,
    pub route_kind: String,
    pub route_value: String,
    pub source: String,
    pub fit_basis_points: u16,
    pub status: String,
    pub refusal_reason: Option<String>,
    pub booking_target_id: Option<uuid::Uuid>,
}
