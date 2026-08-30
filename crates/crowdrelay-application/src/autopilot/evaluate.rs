//! Thin orchestration from typed snapshots to durable decision candidates.

use uuid::Uuid;

use crowdrelay_brain::{
    DispatchPrediction, GrowthIntelligencePolicy, GrowthStrategy, TenantPreferencePolicy,
    context_hash,
};
use crowdrelay_domain::{
    FanId, WorkspaceId,
    action_class::{ActionClass, clamp_disposition},
    audience_lifecycle::{
        FanLifecycleDecision, FanLifecycleSnapshot, LifecycleTemplate, evaluate_fan_lifecycle,
    },
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition, disposition},
    beacons::{
        BeaconCampaignSnapshot, BeaconDecision, BeaconDiscoveryDecision, BeaconDiscoverySnapshot,
        BeaconInviteDecision, BeaconInviteSnapshot, BeaconOutreachPhase, evaluate_beacon_campaign,
        evaluate_beacon_discovery, evaluate_beacon_invite_batch,
    },
    booking::{
        BookingFollowUpDecision, BookingFollowUpPolicy, BookingOpportunityDecision,
        BookingOutreachPhase, BookingTargetDecision, BookingTargetSelectionPolicy,
        BookingTargetSnapshot, CityOpportunitySnapshot, estimated_attendance,
        evaluate_booking_followup, evaluate_booking_opportunity, select_booking_target,
    },
    campaign_lifecycle::{EventCampaignDecision, EventCampaignSnapshot, evaluate_event_campaign},
    content_supply::{ContentSupplyDecision, ContentSupplySnapshot, evaluate_content_supply},
    deliverability::{DeliverabilityPolicy, ramped_ceiling},
    experimentation::{ExperimentDecision, ExperimentSnapshot, evaluate_experiment},
    free_reach::{
        FreeReachPolicy, WaveDecision, WaveSnapshot, WaveState, evaluate_wave, wave_capacity,
        wave_is_worth_opening,
    },
    funding::{FundingDecision, FundingOpportunitySnapshot, evaluate_funding},
    growth_envelope::{EnvelopeUsage, EnvelopeVerdict, GrowthEnvelope, check_envelope},
    live_opportunities::{
        LiveOpportunityDecision, LiveOpportunitySnapshot, evaluate_live_opportunity,
        live_opportunity_score,
    },
    merch_bundle::{MerchBundleDecision, MerchBundleSnapshot, evaluate_merch_bundle},
    merchandising::{
        MerchInventorySnapshot, MerchPriceDecision, MerchPriceDirection, MerchPriceSnapshot,
        MerchReorderDecision, evaluate_merch_price, evaluate_reorder,
    },
    negotiation::{TermsDecision, TermsState, evaluate_terms},
    outreach::{OutreachDecision, OutreachSnapshot, evaluate_outreach},
    play_measurement::measurement_due_at,
    playlist_placement::{PlacementDecision, PlacementPolicy, evaluate_placement},
    plays::{
        PlayDecision, PlayKind, PlayPolicy, PlaySnapshot, StepAudience, evaluate_play,
        play_is_worth_starting, step_schedule,
    },
    pricing::{
        TicketAllocationDecision, TicketYieldDecision, TicketYieldSnapshot,
        evaluate_ticket_allocation, evaluate_ticket_yield,
    },
    promotion::{PromotionBudgetDecision, PromotionPerformanceSnapshot, evaluate_promotion_budget},
    release_autopilot::{
        ReleaseAutopilotPolicy, ReleaseDecision, ReleaseMilestone, ReleasePlanSnapshot,
        evaluate_release,
    },
    show_operations::{ShowOperationsDecision, ShowTaskSnapshot, evaluate_show_task},
    target_discovery::{OutreachSupplyDecision, OutreachSupplySnapshot, evaluate_outreach_supply},
};
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

use super::{model::*, ports::AutopilotDecisionRepository};
mod beacons;
mod booking_supply;
mod commercial;
mod growth_debt;
mod growth_intelligence;
mod growth_metrics;
mod outreach_supply;
mod placements;
mod plays;
mod portfolio;
mod show_growth;

use beacons::{beacon_candidate, beacon_discovery_candidate, beacon_invite_candidate};
use booking_supply::booking_supply_candidate;
use commercial::{
    booking_candidate, booking_followup_candidate, campaign_lifecycle_candidate, funding_candidate,
    merch_candidate, merch_price_candidate,
};
use growth_debt::growth_debt_candidate;
use growth_intelligence::{
    ScoredCandidate, build_dispatch_context, cooldown_window, growth_intelligence_candidate,
};
use growth_metrics::growth_metric_candidate;
use outreach_supply::outreach_supply_candidate;
use plays::{play_decision, play_start, play_step_candidate};
use show_growth::show_growth_candidate;

use crate::RepositoryError;

pub struct EvaluateAutopilot<'a, R> {
    repository: &'a R,
    workspace_id: WorkspaceId,
}

impl<'a, R> EvaluateAutopilot<'a, R>
where
    R: AutopilotDecisionRepository,
{
    #[must_use]
    pub const fn new(repository: &'a R, workspace_id: WorkspaceId) -> Self {
        Self {
            repository,
            workspace_id,
        }
    }

    pub async fn execute(
        &self,
        now: OffsetDateTime,
    ) -> Result<AutopilotCycleReport, AutopilotError> {
        let policies = self.repository.load_policies(self.workspace_id).await?;
        // Loaded once per cycle rather than per candidate: the ceiling is an
        // operator setting that does not change mid-cycle, and re-reading it
        // for every decision would be a query per finding.
        let ceilings = self
            .repository
            .load_autonomy_ceilings(self.workspace_id)
            .await?;
        // Mutable for the whole cycle: the spend is topped up as actions are
        // created, so the cap holds within one cycle and not only across them.
        let (envelope, mut usage) = self
            .repository
            .load_growth_envelope(self.workspace_id, now)
            .await?;
        let touch_ages = self
            .repository
            .load_outward_touch_ages(self.workspace_id, now)
            .await?;
        // Everyone an action reached during this cycle. The cooldown is read
        // from a snapshot taken before the cycle started, so without this a
        // person can be contacted twice inside one pass.
        let mut touched_this_cycle: std::collections::HashSet<uuid::Uuid> =
            std::collections::HashSet::new();
        let mut limits = CycleLimits {
            ceilings: &ceilings,
            envelope: &envelope,
            usage: &mut usage,
            touch_ages: &touch_ages,
            touched_this_cycle: &mut touched_this_cycle,
        };
        let mut report = AutopilotCycleReport::default();

        for policy in policies.into_iter().filter(|policy| policy.enabled) {
            match policy.context {
                AutopilotContext::TicketYield => {
                    let snapshots = self
                        .repository
                        .load_ticket_yield_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = ticket_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                        if let Some(candidate) =
                            ticket_allocation_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::FanLifecycle => {
                    let snapshots = self
                        .repository
                        .load_fan_lifecycle_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = lifecycle_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::CampaignLifecycle => {
                    let snapshots = self
                        .repository
                        .load_event_campaign_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) =
                            campaign_lifecycle_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Merchandising => {
                    let snapshots = self
                        .repository
                        .load_merch_inventory_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = merch_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::MerchPricing => {
                    let snapshots = self
                        .repository
                        .load_merch_price_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = merch_price_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::MerchBundle => {
                    let snapshots = self
                        .repository
                        .load_merch_bundle_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = merch_bundle_candidate(snapshot, &policy)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::BookingOpportunity => {
                    let snapshots = self
                        .repository
                        .load_city_opportunity_snapshots(self.workspace_id, now)
                        .await?;
                    let targets = self
                        .repository
                        .load_booking_target_snapshots(self.workspace_id, now)
                        .await?;
                    for target in &targets {
                        if let Some(candidate) = booking_followup_candidate(target, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    for snapshot in snapshots {
                        if let Some(candidate) =
                            booking_candidate(snapshot, &targets, &policy, now)?
                        {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    let supply = self
                        .repository
                        .load_booking_supply_snapshot(self.workspace_id, now)
                        .await?;
                    if let Some(candidate) =
                        booking_supply_candidate(&supply, &policy, self.workspace_id, now)?
                    {
                        self.persist(&candidate, &mut limits, &mut report).await?;
                    }
                }
                AutopilotContext::Outreach => {
                    // Opening comes first, so a wave created this cycle can
                    // take the pitches the same cycle would otherwise have sent
                    // one at a time.
                    let wave_policy = match policy.config {
                        AutopilotPolicyConfig::Outreach(outreach) => outreach.waves,
                        _ => FreeReachPolicy::default(),
                    };
                    self.open_outreach_waves(&policy, &mut report, now).await?;
                    let mut waves = self
                        .repository
                        .load_outreach_waves(self.workspace_id, now)
                        .await?;
                    let snapshots = self
                        .repository
                        .load_outreach_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        // At most one open wave takes each pitch, and only
                        // while it still has room under the budget it was sized
                        // against. Everything else pitches exactly as before.
                        let wave_id = waves
                            .iter_mut()
                            .find(|wave| {
                                wave.snapshot.target_kind == snapshot.target_kind
                                    && matches!(wave.snapshot.state, WaveState::Drafting)
                                    && matches!(
                                        evaluate_wave(wave.snapshot, wave_policy, now),
                                        WaveDecision::AddPitch
                                    )
                            })
                            .map(|wave| {
                                wave.snapshot.pitches = wave.snapshot.pitches.saturating_add(1);
                                wave.wave_id
                            });
                        if let Some(candidate) =
                            outreach_candidate(snapshot, &policy, wave_id, now)?
                        {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    self.settle_outreach_waves(&waves, wave_policy, &mut report, now)
                        .await?;
                    self.follow_through_placements(&policy, &mut limits, &mut report, now)
                        .await?;
                }
                AutopilotContext::ContentSupply => {
                    let snapshots = self
                        .repository
                        .load_content_supply_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = content_candidate(&snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Experimentation => {
                    let snapshots = self
                        .repository
                        .load_experiment_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = experiment_candidate(&snapshot, &policy)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::ShowOperations => {
                    let snapshots = self
                        .repository
                        .load_show_task_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = show_operations_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::PromotionBudget => {
                    let snapshots = self
                        .repository
                        .load_promotion_performance_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = promotion_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Release => {
                    let snapshots = self
                        .repository
                        .load_release_plan_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = release_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::LiveOpportunity => {
                    let snapshots = self
                        .repository
                        .load_live_opportunity_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = live_opportunity_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    self.advance_live_terms(&policy, &mut limits, &mut report, now)
                        .await?;
                }
                AutopilotContext::Funding => {
                    let snapshots = self
                        .repository
                        .load_funding_opportunity_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = funding_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Beacon => {
                    let discovery = self
                        .repository
                        .load_beacon_discovery_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in discovery {
                        if let Some(candidate) = beacon_discovery_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    let snapshots = self
                        .repository
                        .load_beacon_campaign_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = beacon_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    let invites = self
                        .repository
                        .load_beacon_invite_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in invites {
                        if let Some(candidate) = beacon_invite_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::ShowGrowth => {
                    let snapshots = self
                        .repository
                        .load_show_growth_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = show_growth_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::GrowthMetrics => {
                    let snapshots = self
                        .repository
                        .load_growth_metric_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in &snapshots {
                        if let Some(candidate) = growth_metric_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::OutreachSupply => {
                    let snapshot = self
                        .repository
                        .load_outreach_supply_snapshot(self.workspace_id, now)
                        .await?;
                    if let Some(candidate) =
                        outreach_supply_candidate(&snapshot, &policy, self.workspace_id, now)?
                    {
                        self.persist(&candidate, &mut limits, &mut report).await?;
                    }
                }
                AutopilotContext::Plays => {
                    let AutopilotPolicyConfig::Plays(play_policy) = policy.config else {
                        continue;
                    };
                    // Read once for the whole context: a standing belongs to the
                    // play kind, and re-reading it per show would be a query per
                    // candidate for an answer that cannot change mid-cycle.
                    let standings = self
                        .repository
                        .load_play_standings(self.workspace_id, play_policy)
                        .await?;
                    let standing_for = |kind: PlayKind| {
                        standings
                            .iter()
                            .find(|standing| standing.kind == kind)
                            .copied()
                    };

                    // Starting comes first so a play created this cycle can run
                    // a step that is already due. An announce step for a show
                    // announced fourteen days late is due the moment its play
                    // exists, and making it wait a cycle for no reason is a
                    // cycle of its window spent.
                    for kind in PlayKind::all() {
                        // A retired kind is proposed no longer. Retirement bites
                        // here and only here: a campaign already committed to a
                        // specific show finishes under the ceilings it started
                        // with, because abandoning it mid-run would leave steps
                        // that nothing ever settles.
                        if standing_for(kind).is_some_and(|standing| standing.standing.is_retired())
                        {
                            continue;
                        }
                        let anchors = self
                            .repository
                            .load_play_anchors(self.workspace_id, kind, now)
                            .await?;
                        for anchor in anchors {
                            let Some(start) = play_start(kind, anchor, &policy) else {
                                continue;
                            };
                            if self
                                .repository
                                .start_play(self.workspace_id, &start)
                                .await?
                            {
                                report.plays_started = report.plays_started.saturating_add(1);
                            }
                        }
                    }
                    let snapshots = self
                        .repository
                        .load_play_snapshots(self.workspace_id, now)
                        .await?;
                    for mut snapshot in snapshots {
                        // The record narrows the reach of a running play and
                        // never widens it. A retired kind keeps its configured
                        // ceiling so the campaign in flight can still settle.
                        let narrowed = standing_for(snapshot.kind)
                            .filter(|standing| !standing.standing.is_retired())
                            .map_or(policy.clone(), |standing| AutopilotPolicy {
                                config: AutopilotPolicyConfig::Plays(PlayPolicy {
                                    max_recipients_per_step: standing
                                        .effective_max_recipients_per_step,
                                    ..play_policy
                                }),
                                ..policy.clone()
                            });
                        self.advance_play(&mut snapshot, &narrowed, &mut limits, &mut report, now)
                            .await?;
                    }
                }
                AutopilotContext::GrowthDebt => {
                    let observations = self
                        .repository
                        .load_growth_debt_observations(self.workspace_id, now)
                        .await?;
                    for observation in &observations {
                        if let Some(candidate) = growth_debt_candidate(observation, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::GrowthIntelligence => {
                    self.evaluate_growth_intelligence_context(
                        &policy,
                        now,
                        &mut limits,
                        &mut report,
                    )
                    .await?;
                }
            }
        }

        Ok(report)
    }

    /// Carries every claimed placement through to something that can be
    /// counted, or to something that cannot.
    ///
    /// This is the anti-scam core. Nothing here takes a curator's word: a claim
    /// counts toward no report until a public read confirms it, and a
    /// confirmation that disappears inside the window suppresses the operator
    /// behind it rather than the playlist it happened in.
    async fn follow_through_placements(
        &self,
        policy: &AutopilotPolicy,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        placements::follow_through_placements(self, policy, limits, report, now).await
    }

    async fn settle_outreach_waves(
        &self,
        waves: &[OutreachWaveSnapshot],
        policy: FreeReachPolicy,
        report: &mut AutopilotCycleReport,
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        for wave in waves {
            let transition = match evaluate_wave(wave.snapshot, policy, now) {
                WaveDecision::Seal => OutreachWaveTransition::Seal,
                WaveDecision::Expire { reason } => OutreachWaveTransition::Expire { reason },
                WaveDecision::AddPitch | WaveDecision::Hold(_) => continue,
            };
            self.repository
                .transition_outreach_wave(self.workspace_id, wave.wave_id, transition, now)
                .await?;
            match transition {
                OutreachWaveTransition::Seal => {
                    report.waves_sealed = report.waves_sealed.saturating_add(1);
                }
                OutreachWaveTransition::Expire { .. } => {
                    report.waves_expired = report.waves_expired.saturating_add(1);
                }
            }
        }
        Ok(())
    }

    /// Moves every live negotiation on by at most one step.
    ///
    /// Settlements are written straight to the row rather than queued as
    /// actions. A decline is the agent recording that it will not take these
    /// terms, and an unrecorded refusal reads to an operator exactly like the
    /// agent never looking.
    async fn advance_live_terms(
        &self,
        policy: &AutopilotPolicy,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        let AutopilotPolicyConfig::LiveOpportunity(domain_policy) = policy.config else {
            return Ok(());
        };
        for snapshot in self
            .repository
            .load_live_opportunity_terms(self.workspace_id, now)
            .await?
        {
            let score = live_opportunity_score(snapshot.opportunity);
            match evaluate_terms(
                snapshot.terms,
                snapshot.opportunity,
                domain_policy,
                score,
                now,
            ) {
                TermsDecision::Hold => {}
                TermsDecision::Decline { reason } => {
                    self.repository
                        .settle_live_opportunity_terms(
                            self.workspace_id,
                            &TermsSettlement {
                                opportunity_id: snapshot.terms.opportunity_id,
                                state: TermsState::Declined,
                                reason: Some(reason),
                            },
                            now,
                        )
                        .await?;
                    report.terms_settled = report.terms_settled.saturating_add(1);
                }
                TermsDecision::Expire => {
                    self.repository
                        .settle_live_opportunity_terms(
                            self.workspace_id,
                            &TermsSettlement {
                                opportunity_id: snapshot.terms.opportunity_id,
                                state: TermsState::Expired,
                                reason: None,
                            },
                            now,
                        )
                        .await?;
                    report.terms_settled = report.terms_settled.saturating_add(1);
                }
                TermsDecision::Counter { .. } | TermsDecision::Accept { .. } => {
                    if let Some(candidate) = live_terms_candidate(&snapshot, policy, now)? {
                        self.persist(&candidate, limits, report).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Walks one running play as far as it will go this cycle.
    ///
    /// A settle can make the step behind it immediately actionable: a withdrawn
    /// anchor settles every remaining step in turn, and an expired step settles
    /// one that is already due. So this loops rather than deciding once — but
    /// only ever forward, and never more times than there are steps, because a
    /// state machine that stopped shrinking would otherwise spin against the
    /// database for the rest of the cycle.
    async fn advance_play(
        &self,
        snapshot: &mut PlayRunSnapshot,
        policy: &AutopilotPolicy,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        let bound = snapshot.steps.len().saturating_add(1);
        for _ in 0..bound {
            let Some(decision) = play_decision(snapshot, policy, now) else {
                return Ok(());
            };
            match decision {
                PlayDecision::Hold(_) => return Ok(()),
                PlayDecision::RunStep { .. } => {
                    if let Some(candidate) = play_step_candidate(snapshot, decision, policy)? {
                        self.persist(&candidate, limits, report).await?;
                    }
                    // One send per play per cycle. The recipient came from a
                    // read taken before this action existed, and deciding again
                    // against it would either offer the same fan twice or skip
                    // the next one; the following cycle reads a fresh audience.
                    return Ok(());
                }
                PlayDecision::SkipStep { index, reason, .. } => {
                    self.repository
                        .settle_play_step(
                            self.workspace_id,
                            &PlayStepSettlement {
                                play_id: snapshot.play_id,
                                step_index: index,
                                reason,
                            },
                            now,
                        )
                        .await?;
                    report.play_steps_skipped = report.play_steps_skipped.saturating_add(1);
                    // The same settle applied to the copy in hand. Without it
                    // the next pass reads the step as still open and settles it
                    // again, which double-counts an omission that happened once.
                    if let Some(step) = snapshot
                        .steps
                        .iter_mut()
                        .find(|step| step.index == index && !step.settled)
                    {
                        step.settled = true;
                    }
                }
                PlayDecision::Complete => {
                    self.repository
                        .complete_play(self.workspace_id, snapshot.play_id, now)
                        .await?;
                    report.plays_completed = report.plays_completed.saturating_add(1);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// The one place every candidate passes through, and therefore the only
    /// place the class ceiling has to be applied.
    ///
    /// Doing it here rather than inside each of the twenty candidate functions
    /// means a new detector cannot forget it, and a detector author cannot
    /// choose to skip it.
    ///
    /// Returns the action_id if one was created or already existed. Callers
    /// that don't need it can ignore the return value.
    async fn persist(
        &self,
        candidate: &DecisionCandidate,
        limits: &mut CycleLimits<'_>,
        report: &mut AutopilotCycleReport,
    ) -> Result<Option<Uuid>, AutopilotError> {
        let class = candidate.action.action_class();
        let ceiling = limits
            .ceilings
            .iter()
            .find_map(|(known, level)| (*known == class).then_some(*level))
            // An absent row is the safest ceiling, never an absent limit.
            .unwrap_or_else(|| class.safest_ceiling());
        let clamped = clamp_disposition(candidate.disposition, ceiling);
        if clamped != candidate.disposition {
            report.actions_gated = report.actions_gated.saturating_add(1);
        }

        // The volume limits apply after the class ceiling, never instead of it:
        // a full budget must not be able to let a third-party action through,
        // and an empty one must not promote anything.
        let subject_usage = EnvelopeUsage {
            // Only contacts have a cooldown. An event is a topic, not a person:
            // keying it there would let one show run a single growth lever a
            // week and quietly starve the other nine.
            hours_since_subject_touched: candidate
                .subject
                .is_contactable_person()
                .then(|| {
                    // Somebody this cycle already reached is touched now, not
                    // whenever the cycle's snapshot says. Without this, two
                    // contexts — or two plays around two different shows — can
                    // each pass the cooldown against the same stale reading and
                    // between them message one person twice in a minute.
                    if limits
                        .touched_this_cycle
                        .contains(&candidate.subject.uuid())
                    {
                        return Some(0);
                    }
                    limits.touch_ages.get(&candidate.subject.uuid()).copied()
                })
                .flatten(),
            ..*limits.usage
        };
        let clamped = match check_envelope(class, limits.envelope, &subject_usage) {
            EnvelopeVerdict::Allow => clamped,
            EnvelopeVerdict::Hold(block) => {
                report.actions_held = report.actions_held.saturating_add(1);
                // A rehearsal produces the decision and its evidence but
                // nothing anybody can press send on. Every other block still
                // offers the work to a human, because "the budget is spent" is
                // not the same as "this should not happen".
                if block.may_offer_for_approval() {
                    clamp_disposition(clamped, AutonomyLevel::RequireApproval)
                } else {
                    clamp_disposition(clamped, AutonomyLevel::Recommend)
                }
            }
        };

        let candidate = &DecisionCandidate {
            disposition: clamped,
            ..candidate.clone()
        };
        let persisted = self
            .repository
            .persist_candidate(self.workspace_id, candidate)
            .await?;
        if persisted.decision_created {
            report.decisions = report.decisions.saturating_add(1);
        }
        if persisted.action_created {
            report.actions_enqueued = report.actions_enqueued.saturating_add(1);
            if candidate.subject.is_contactable_person() {
                limits.touched_this_cycle.insert(candidate.subject.uuid());
            }
            // Spend the budget as it is used, not once at the start of the
            // cycle. Without this the weekly cap is read from a snapshot that
            // never moves, and a single cycle with fifty findings enqueues all
            // fifty against a budget of five.
            match class {
                ActionClass::OwnedAudience => {
                    limits.usage.owned_audience_touches_7d =
                        limits.usage.owned_audience_touches_7d.saturating_add(1);
                }
                ActionClass::ThirdParty => {
                    limits.usage.third_party_touches_7d =
                        limits.usage.third_party_touches_7d.saturating_add(1);
                }
                ActionClass::FirstPartyReversible | ActionClass::Paid => {}
            }
        }
        if persisted.quota_throttled {
            report.actions_throttled = report.actions_throttled.saturating_add(1);
        }
        Ok(persisted.action_id)
    }
}

/// Deterministic pseudo-random roll from a string key. Maps the key's
/// hash to [0, 1). Used for the randomized holdout — the same decision
/// key always gets the same roll within one cycle, preventing flapping.
fn deterministic_roll(key: &str) -> f64 {
    // FNV-1a hash → u64 → [0, 1).
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in key.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    // Map to [0, 1) using the upper 53 bits (mantissa precision of f64).
    f64::from_bits(0x3FF0_0000_0000_0000 | (hash & 0x000F_FFFF_FFFF_FFFF)) - 1.0
}

include!("evaluate/types.rs");
include!("evaluate/candidates.rs");
include!("evaluate/growth_intelligence_context.rs");
include!("evaluate/tests.rs");
include!("evaluate/growth_metrics_tests.rs");
include!("evaluate/growth_debt_tests.rs");
include!("evaluate/plays_tests.rs");
