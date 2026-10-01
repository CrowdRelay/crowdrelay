//! Thin orchestration from typed snapshots to durable decision candidates.

use crowdrelay_brain::{
    DispatchPrediction, GrowthIntelligencePolicy, GrowthStrategy, context_hash,
};
use crowdrelay_domain::{
    FanId, TraceContext, WorkspaceId,
    action_class::{ActionClass, clamp_disposition},
    audience_lifecycle::{
        FanLifecycleDecision, FanLifecycleSnapshot, LifecycleTemplate, evaluate_fan_lifecycle,
    },
    autonomy::{
        AutonomyLevel, Confidence, ContextEvidence, PolicyDisposition, disposition,
        disposition_with_evidence,
    },
    beacons::{
        BeaconCampaignSnapshot, BeaconDecision, BeaconDiscoveryDecision, BeaconDiscoverySnapshot,
        BeaconInviteDecision, BeaconInviteSnapshot, BeaconOutreachPhase, evaluate_beacon_campaign,
        evaluate_beacon_discovery, evaluate_beacon_invite_batch,
    },
    booking::{
        BookingFollowUpDecision, BookingFollowUpPolicy, BookingOpportunityDecision,
        BookingOutreachPhase, BookingTargetDecision, BookingTargetSelectionPolicy,
        BookingTargetSnapshot, CityOpportunitySnapshot, FestivalWindowDecision,
        FestivalWindowPolicy, additional_booking_recipients, estimated_attendance,
        evaluate_booking_followup, evaluate_booking_opportunity, evaluate_festival_window,
        select_booking_target,
    },
    booking_venue_fit::{VenueFitDecision, VenueFitPolicy, evaluate_venue_fit},
    booking_window::{BookingWindowInputSet, BookingWindowInputs, propose_booking_window},
    campaign_lifecycle::{EventCampaignDecision, EventCampaignSnapshot, evaluate_event_campaign},
    content_supply::{
        CommunityRelayTarget, ContentSourceKind, ContentSupplyDecision, ContentSupplyHoldReason,
        ContentSupplySnapshot, evaluate_content_supply,
    },
    deliverability::{DeliverabilityPolicy, ramped_ceiling},
    experimentation::{ExperimentDecision, ExperimentSnapshot, evaluate_experiment},
    free_reach::{
        FreeReachPolicy, WaveAnchor, WaveDecision, WaveSnapshot, WaveState, evaluate_wave,
        wave_capacity, wave_is_worth_opening,
    },
    funding::{FundingDecision, FundingOpportunitySnapshot, evaluate_funding},
    growth_envelope::{
        EnvelopeUsage, EnvelopeVerdict, GrowthEnvelope, check_attention, check_envelope,
    },
    learning::Standing,
    live_opportunities::{
        LiveOpportunityDecision, LiveOpportunitySnapshot, evaluate_live_opportunity,
        live_opportunity_score,
    },
    measurement::RATE_FLOOR,
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
        ShowWeekCollision, evaluate_release,
    },
    show_operations::{ShowOperationsDecision, ShowTaskSnapshot, evaluate_show_task},
    target_discovery::{OutreachSupplyDecision, OutreachSupplySnapshot, evaluate_outreach_supply},
};
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use super::ports::{AutopilotDecisionRepository, LoadedCausalModel};
use super::{evidence_ledger::EvidenceLedger, model::*, policy_config::*};
mod beacons;
mod booking_supply;
mod commercial;
mod content_strategy;
mod growth_debt;
mod growth_intelligence;
mod growth_metrics;
mod join_ask;
mod outreach_supply;
mod placements;
mod plays;
mod portfolio;
mod show_growth;

use beacons::{beacon_candidate, beacon_discovery_candidate, beacon_invite_candidate};
use booking_supply::booking_supply_candidate;
use commercial::{
    booking_candidate, booking_followup_candidate, campaign_lifecycle_candidate,
    festival_window_candidate, funding_candidate, merch_candidate, merch_price_candidate,
    venue_fit_candidate,
};
use content_strategy::{content_arc_candidate, content_strategy_candidate};
use crowdrelay_domain::worker_template::WorkerTemplate;
use growth_debt::growth_debt_candidate;
use growth_intelligence::{
    ScoredCandidate, build_dispatch_context, cooldown_window, growth_intelligence_candidate,
};
use growth_metrics::growth_metric_candidate;
use join_ask::evaluate_join_ask_candidates;
use outreach_supply::outreach_supply_candidate;
use plays::{play_decision, play_start, play_step_candidate};
use show_growth::show_growth_candidates;

use crate::RepositoryError;

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
        // What each context has actually learned, on the same once-per-cycle
        // terms as the ceilings above. Until this existed the authority gate
        // could ask a context how confident it was but not what that
        // confidence was computed from, so a context with four measured
        // outcomes could report a high number, clear its minimum, and be
        // handed unattended execution over an action nobody can recall.
        let evidence_counts = self
            .repository
            .load_resolved_evidence_counts(self.workspace_id)
            .await?;
        // Mutable for the whole cycle: the spend is topped up as actions are
        // created, so the cap holds within one cycle and not only across them.
        let (envelope, mut usage) = self
            .repository
            .load_growth_envelope(self.workspace_id, now)
            .await?;
        // The warm-up: how much unattended action each context may still take
        // while below its floor. Without it the floor seals itself — acting is
        // how the observations that clear it get made, and routing below-floor
        // work through approval instead of denial made no difference against
        // one operator and a 72-hour expiry. Measured: zero resolved outcomes
        // against a floor of twenty.
        let bootstrap_spent = self
            .repository
            .load_bootstrap_spend(self.workspace_id, now)
            .await?;
        let evidence =
            evidence_counts.with_bootstrap(bootstrap_spent, envelope.weekly_bootstrap_actions);
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

        let loaded_causal_model = self.load_cycle_causal_model(&policies, &mut report).await?;
        // Checkpoint replay before unrelated contexts can abort; the model is read-only below.
        self.checkpoint_cycle_causal_model(loaded_causal_model.as_ref(), &mut report)
            .await;

        for policy in policies.into_iter().filter(|policy| policy.enabled) {
            // Registered before the arm runs so a silent detector still
            // reports `candidates: 0` — a reading `decisions` cannot show.
            report.context_stats(policy.context);
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
                    for snapshot in &snapshots {
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
                    // §12-6: the room history + own-calendar set the window
                    // proposal reads. Loaded once per cycle — the own-show
                    // half is the same for every city under review, and
                    // without targets there is nobody to propose a window
                    // for, so the read is skipped entirely.
                    let window_inputs = if targets.is_empty() {
                        BookingWindowInputSet::default()
                    } else {
                        self.repository
                            .load_booking_window_inputs(self.workspace_id, now)
                            .await?
                    };
                    // Festival windows propose first: a closing application
                    // is the most perishable ask a target can carry. A target
                    // the deadline just proposed is excluded from both the
                    // follow-up (the new letter is the fresh ask, the nudge
                    // would be stale) and the city path (one letter per
                    // target per cycle — the deadline wins over demand).
                    let mut festival_proposed = std::collections::HashSet::new();
                    for target in &targets {
                        if let Some(candidate) = festival_window_candidate(target, &policy, now)? {
                            festival_proposed.insert(target.target_id);
                            self.persist(&candidate, &mut limits, &mut report).await?;
                            continue;
                        }
                        if let Some(candidate) = booking_followup_candidate(target, &policy, now)? {
                            // One letter per target per cycle: a live
                            // follow-up means the cold-start fit lane
                            // stands down even if the room evidence would
                            // have qualified on its own.
                            festival_proposed.insert(target.target_id);
                            self.persist(&candidate, &mut limits, &mut report).await?;
                            continue;
                        }
                        if let Some(candidate) = venue_fit_candidate(target, &policy, now)? {
                            // Fit asks are per-target cold-start proposals,
                            // not city asks — they do not exclude the
                            // target from the demand path's recipient set.
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    let city_targets: Vec<BookingTargetSnapshot> = targets
                        .iter()
                        .filter(|target| !festival_proposed.contains(&target.target_id))
                        .cloned()
                        .collect();
                    for snapshot in snapshots {
                        if let Some(candidate) = booking_candidate(
                            snapshot,
                            &city_targets,
                            &window_inputs,
                            &policy,
                            now,
                        )? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    let supply = self
                        .repository
                        .load_booking_supply_snapshot(self.workspace_id, now)
                        .await?;
                    if let Some(candidate) = booking_supply_candidate(
                        &supply,
                        &policy,
                        evidence.for_context(policy.context),
                        self.workspace_id,
                        now,
                    )? {
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
                    // The reply probability model. Loaded once per cycle; its
                    // P(positive reply) is logged on every candidate and, below,
                    // orders the pitches. It does NOT change eligibility,
                    // disposition or action. A load failure or cold start
                    // produces the global prior for every target, which is a
                    // no-op for the (stable) ordering.
                    let reply_model = self
                        .repository
                        .load_reply_model(self.workspace_id)
                        .await
                        .unwrap_or_default();
                    // The model ranks who is pitched first, and nothing else.
                    // A wave holds a dozen pitches and the quota twenty a day,
                    // while one catalogue pitch reaches two hundred contacts,
                    // so the order decides who is asked at all. Asking first
                    // the contacts most likely to answer, by kind and by their
                    // own history, is what the model learns for. Eligibility,
                    // cadence, confidence and disposition still come only from
                    // the deterministic evaluation. The sort is stable: with a
                    // cold model every prediction is the prior and the loader's
                    // order stands.
                    let mut snapshots = snapshots;
                    snapshots.sort_by(|left, right| {
                        let p = |snapshot: &crowdrelay_domain::outreach::OutreachSnapshot| {
                            reply_model
                                .predict(
                                    snapshot.target_kind.as_str(),
                                    &snapshot.target_id.to_string(),
                                )
                                .probability
                        };
                        p(right).total_cmp(&p(left))
                    });
                    for snapshot in snapshots {
                        // Extract target identifiers before the snapshot is
                        // moved into outreach_candidate, so the shadow
                        // prediction can use them.
                        let kind_str = snapshot.target_kind.as_str();
                        let target_id_str = snapshot.target_id.to_string();
                        // At most one open wave takes each pitch, and only
                        // while it still has room under the budget it was sized
                        // against. Everything else pitches exactly as before.
                        // A threads wave is its own lane: a follow-up on a
                        // hand-started thread must not draft into a release or
                        // catalogue batch, and a pitch must not ride the
                        // threads wave it has nothing to do with.
                        let wave_id = waves
                            .iter_mut()
                            .find(|wave| {
                                wave.snapshot.target_kind == snapshot.target_kind
                                    && matches!(wave.snapshot.anchor, WaveAnchor::Threads { .. })
                                        == snapshot.thread_followup
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
                        // Wave-only supply waits for a wave with room rather
                        // than arriving as its own approval card.
                        if snapshot.wave_only && wave_id.is_none() {
                            continue;
                        }
                        if let Some(mut candidate) =
                            outreach_candidate(snapshot, &policy, wave_id, now)?
                        {
                            // Shadow prediction: log P(positive reply) alongside
                            // the existing input_snapshot. This does NOT change
                            // the candidate's confidence, disposition, action,
                            // or decision_key — those are all derived from the
                            // deterministic outreach evaluation, not from the
                            // input_snapshot JSON.
                            let shadow_pred = reply_model.predict(kind_str, &target_id_str);
                            if let Some(obj) = candidate.input_snapshot.as_object_mut() {
                                obj.insert(
                                    "shadow_reply_prediction".to_owned(),
                                    serde_json::to_value(&shadow_pred)
                                        .unwrap_or(serde_json::Value::Null),
                                );
                            }
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    // The other half of the conversation: every inbound reply
                    // with no answer is work, not a stop — the pitch loop's
                    // `AlreadyReplied` hold refuses them, this lane answers.
                    // Replies run after the pitches: a cycle that can only
                    // afford one kind of letter owes the answer first in
                    // spirit, and the per-context cap orders what persists.
                    let reply_snapshots = self
                        .repository
                        .load_unanswered_reply_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in reply_snapshots {
                        if let Some(candidate) = reply_rescue_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    // Save the reply model checkpoint for fast startup on
                    // the next cycle. Best-effort: a failure just means the
                    // next cycle rebuilds from full history.
                    let _ = self
                        .repository
                        .save_reply_model(self.workspace_id, &reply_model)
                        .await;
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
                    // The stop rule: no live material is not a quiet
                    // portfolio, it is the band having done nothing public
                    // lately — `supply_quiet_reason` names which quiet the
                    // cycle is in rather than reporting a generic "nothing
                    // scored". A non-supply config still skips the loop.
                    if !matches!(policy.config, AutopilotPolicyConfig::ContentSupply(_)) {
                        continue;
                    }
                    // Admitted communities — and the push audience the
                    // approval will quote — are loaded once, and only when a
                    // fresh synced post could be relayed or a fresh drop
                    // could surge — a cycle with neither owes either read
                    // nothing.
                    let domain_policy = match &policy.config {
                        AutopilotPolicyConfig::ContentSupply(config) => Some(*config),
                        _ => None,
                    };
                    let has_relay_material = snapshots.iter().any(|snapshot| {
                        snapshot.source_kind == ContentSourceKind::SocialPost
                            || domain_policy.is_some_and(|config| {
                                crowdrelay_domain::content_supply::drop_surge_eligible(
                                    snapshot, &config, now,
                                )
                            })
                    });
                    let communities = if has_relay_material {
                        self.repository
                            .load_relay_community_targets(self.workspace_id)
                            .await?
                    } else {
                        Vec::new()
                    };
                    let push_audience = if has_relay_material {
                        Some(
                            self.repository
                                .load_signal_push_audience(self.workspace_id, None)
                                .await?,
                        )
                    } else {
                        None
                    };
                    // Relay pushes are paced across cycles and within one:
                    // see `relay_push_verdict`. Newest post first, so a held
                    // backlog relays the freshest news, not the oldest.
                    let mut recent_relays = if has_relay_material {
                        self.repository
                            .load_recent_relay_pushes(
                                self.workspace_id,
                                now - time::Duration::days(
                                    crowdrelay_domain::content_supply::RELAY_PUSH_DEDUPE_DAYS,
                                ),
                            )
                            .await?
                    } else {
                        Vec::new()
                    };
                    let mut ordered: Vec<&ContentSupplySnapshot> = snapshots.iter().collect();
                    ordered.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.occurred_at));
                    let mut produced = 0usize;
                    for snapshot in ordered {
                        for candidate in content_candidates(
                            snapshot,
                            &policy,
                            &communities,
                            push_audience,
                            evidence.for_context(policy.context),
                            now,
                        )? {
                            let relay_push = match &candidate.action {
                                AutopilotActionPayload::RequestSignalPush {
                                    title, body, ..
                                } if candidate.decision_kind == "relay_owned_post" => {
                                    Some((title.clone(), body.clone()))
                                }
                                _ => None,
                            };
                            if let Some((title, body)) = &relay_push
                                && crowdrelay_domain::content_supply::relay_push_verdict(
                                    title,
                                    body,
                                    &recent_relays,
                                    now,
                                ) != crowdrelay_domain::content_supply::RelayPushVerdict::Send
                            {
                                continue;
                            }
                            produced += 1;
                            let action = self.persist(&candidate, &mut limits, &mut report).await?;
                            if let (Some(_), Some((title, body))) = (action, relay_push) {
                                recent_relays.push(
                                    crowdrelay_domain::content_supply::RecentRelayPush {
                                        at: now,
                                        title,
                                        body,
                                    },
                                );
                            }
                        }
                    }
                    report.supply_wait_reason =
                        supply_quiet_reason(&snapshots, &policy, produced, now);
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
                    // §4i-2: loaded once per cycle — the week that holds a live
                    // show is the same week for every release under review.
                    // Skipped entirely when no release is active: a tenant
                    // with nothing to protect owes the query nothing.
                    let collisions = if snapshots.is_empty() {
                        Vec::new()
                    } else {
                        self.repository
                            .load_colliding_show_week(self.workspace_id, now)
                            .await?
                    };
                    for snapshot in snapshots {
                        if let Some(candidate) =
                            release_candidate(snapshot, &policy, now, &collisions)?
                        {
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
                    self.evaluate_beacon_campaigns(
                        &policy,
                        loaded_causal_model.as_ref(),
                        &mut limits,
                        &mut report,
                        now,
                    )
                    .await?;
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
                    // Measured standing per lever — the generalized form of
                    // what agent templates already get. A lever whose own
                    // outcomes retired it is refused here, before the domain
                    // ladder can propose it again for the next event.
                    let standings = self
                        .repository
                        .load_action_standings(self.workspace_id)
                        .await?;
                    let external_executor_live = self
                        .repository
                        .capability_serviceable(self.workspace_id, "show.growth")
                        .await?;
                    let failures = self
                        .repository
                        .load_show_growth_failures(self.workspace_id)
                        .await?;
                    for snapshot in snapshots {
                        // One snapshot can emit two candidates: the §4e-2
                        // refusal of an unreciprocated crossbill lever, and —
                        // the refusal belongs to that lever, not the ladder —
                        // the next lever that is due on its own schedule.
                        for candidate in show_growth_candidates(
                            snapshot,
                            &policy,
                            evidence.for_context(policy.context),
                            &standings,
                            external_executor_live,
                            &failures,
                            now,
                        )? {
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
                        if let Some(candidate) = growth_metric_candidate(
                            snapshot,
                            &policy,
                            evidence.for_context(policy.context),
                            now,
                        )? {
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
                        if let Some(candidate) = growth_debt_candidate(
                            observation,
                            &policy,
                            evidence.for_context(policy.context),
                            now,
                        )? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::GrowthIntelligence => {
                    let Some(loaded_model) = loaded_causal_model.as_ref() else {
                        return Err(RepositoryError::Unexpected.into());
                    };
                    self.evaluate_growth_intelligence_context(
                        &policy,
                        &evidence,
                        loaded_model,
                        now,
                        &mut limits,
                        &mut report,
                    )
                    .await?;
                }
                AutopilotContext::ContentStrategy => {
                    // The season's shape asks first: an arc the band has not
                    // answered outranks the beats that would fill it — until
                    // the shape is chosen, beats are noise.
                    for arc in &self
                        .repository
                        .load_proposed_content_arcs(self.workspace_id, now)
                        .await?
                    {
                        if let Some(candidate) = content_arc_candidate(arc, &policy)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    let suggestions = self
                        .repository
                        .load_open_content_suggestions(self.workspace_id, now)
                        .await?;
                    for suggestion in &suggestions {
                        if let Some(candidate) = content_strategy_candidate(suggestion, &policy)? {
                            self.persist(&candidate, &mut limits, &mut report).await?;
                        }
                    }
                    // §5: the weekly join-ask rides this context — a post on
                    // the band's own pages in the band's own words is the
                    // strategy surface's work. Evaluated for every tenant,
                    // including one that has written nothing: it is held on
                    // `NoVariants` rather than skipped, so a workspace nobody
                    // has set up reports what it is waiting on instead of
                    // producing a cycle that reads as healthy and empty.
                    let snapshot = self
                        .repository
                        .load_join_ask_snapshot(self.workspace_id, now)
                        .await?;
                    let evaluation =
                        evaluate_join_ask_candidates(&snapshot, &policy, self.workspace_id, now)?;
                    report.join_ask_held.extend(evaluation.held);
                    for candidate in &evaluation.candidates {
                        self.persist(candidate, &mut limits, &mut report).await?;
                    }
                }
                AutopilotContext::Representation | AutopilotContext::BookingAgent => {
                    // Approaches are band-initiated: the evaluator never
                    // proposes one. The policy rows hold posture for a human
                    // to read and change — a cycle volunteering the band's
                    // name to an agent is the blast the season rule exists
                    // to prevent.
                }
                AutopilotContext::Roster => {
                    // Roster handoffs are sweep-issued: the weekly brief is
                    // composed and queued by the worker's roster sweep, not
                    // proposed per-workspace here. The policy row holds
                    // posture for a human to read and change.
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
}

include!("evaluate/persist.rs");
include!("evaluate/live_terms.rs");
include!("evaluate/types.rs");
include!("evaluate/candidates.rs");
include!("evaluate/candidates_lifecycle.rs");
include!("evaluate/candidates_terms.rs");
include!("evaluate/candidates_relay.rs");
include!("evaluate/candidates_drop_surge.rs");
include!("evaluate/candidates_reply_rescue.rs");
include!("evaluate/supply_quiet.rs");
include!("evaluate/growth_intelligence_context.rs");
include!("evaluate/causal_cycle.rs");
include!("evaluate/beacon_learning.rs");
include!("evaluate/hypothesis_validation.rs");
include!("evaluate/tests.rs");
include!("evaluate/tests_booking.rs");
include!("evaluate/growth_metrics_tests.rs");
include!("evaluate/join_ask_tests.rs");
include!("evaluate/growth_debt_tests.rs");
include!("evaluate/content_strategy_tests.rs");
include!("evaluate/plays_tests.rs");
include!("evaluate/play_advance.rs");
include!("evaluate/support.rs");
