//! Portfolio optimization for growth intelligence dispatch.
//!
//! Extracts the portfolio candidate construction and optimizer call
//! from the main evaluator loop. The portfolio optimizer selects the
//! optimal set of dispatch candidates, accounting for audience overlap
//! and fatigue.
//!
//! Each candidate carries a `DecisionValue` — the canonical intrinsic
//! value object. The optimizer computes marginal value from it after
//! applying portfolio interactions (overlap, fatigue, budget).

use std::collections::{HashMap, HashSet};

use crowdrelay_brain::{
    DecisionMode, DecisionValue, EfeSignal, GrowthIntelligencePolicy, OpportunityAction,
    OpportunityId, PortfolioCandidate, PortfolioConfig, PortfolioOptimizer, PortfolioSelection,
    ResourceCost, WaitCandidateValue, context_hash,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::worker_template::{TemplateAudience, WorkerTemplate};

use crate::autopilot::evaluate::growth_intelligence::ScoredCandidate;
use crate::autopilot::model::DecisionCandidate;

/// P1-e: Extracts the audience identity from a candidate's decision_key.
///
/// The audience_key must be target-only (not template+target) so that two
/// different templates hitting the same community are detected as audience
/// overlap. The action differs; the audience doesn't.
///
/// - community-engager: `community:{target_id}` (extracted from decision_key
///   segment 4)
/// - workspace-wide templates: `workspace:{workspace_id}` (they all hit the
///   same audience — the workspace)
/// - other templates: `target:{decision_key}` (fallback — each decision_key
///   is a unique target)
fn audience_key_for(candidate: &DecisionCandidate, workspace_id: WorkspaceId) -> String {
    let parts: Vec<&str> = candidate.decision_key.split(':').collect();
    // decision:growth-intelligence:v{N}:{template}:{target_id}:{bucket}
    let template = parts.get(3).and_then(|id| WorkerTemplate::parse(id));
    match template.map(WorkerTemplate::audience) {
        Some(TemplateAudience::Community) => match parts.get(4) {
            Some(target_id) => format!("community:{target_id}"),
            // A community template with no target in its key is malformed.
            // Falling back to the decision key keeps it uniquely identified
            // rather than pooling it with an unrelated community.
            None => format!("target:{}", candidate.decision_key),
        },
        // Every workspace-wide dispatch reaches the same audience — the
        // band's own — so they share a key and the overlap penalty applies
        // between them. This used to be seven string literals, and
        // `discord-poster` was not among them: a discord post would have
        // counted as reaching different people than the telegram and social
        // posts going to the same channels on the same day.
        Some(TemplateAudience::Workspace) => {
            format!("workspace:{}", workspace_id.into_uuid())
        }
        // A scan reaches nobody, so it fatigues nobody. Its own key keeps it
        // out of the band audience's overlap accounting in both directions:
        // a scan does not suppress the posts behind it, and posts do not
        // suppress a scan.
        Some(TemplateAudience::Intelligence) => match template {
            Some(t) => format!("intelligence:{}", t.as_str()),
            None => format!("target:{}", candidate.decision_key),
        },
        // Not a growth-intelligence template: each decision key is its own
        // target.
        None => format!("target:{}", candidate.decision_key),
    }
}

/// Returns the operator-configured resource cost for a template, as a
/// `ResourceCost` with `CostSource::Configured`. Falls back to 1.0 when
/// the template is not in the policy's cost map.
fn template_cost(policy: &GrowthIntelligencePolicy, template_id: &str) -> ResourceCost {
    ResourceCost::configured(policy.template_cost(template_id))
}

/// Builds portfolio candidates from scored growth intelligence candidates
/// and runs the optimizer to select the optimal dispatch set.
///
/// Each candidate is constructed with a `DecisionValue` — the canonical
/// intrinsic value object that carries the full provenance trail:
/// estimation regime, evidence quality, uncertainty, bridge confidence.
/// The optimizer reads `decision_value.total()` for ranking and applies
/// portfolio interactions (overlap, fatigue, budget) to compute marginal
/// value.
///
/// `pending_measurement_count` is the number of unresolved evidence rows
/// (dispatches whose outcomes haven't been observed yet). This feeds the
/// WAIT candidate's value-of-information computation. When > 0, WAIT
/// has real epistemic value — the brain can learn from pending outcomes
/// before committing to new dispatches.
///
/// Returns the portfolio selection. The caller should only dispatch
/// candidates whose `decision_key` appears in the selection's `selected`
/// list, and skip all candidates if `do_nothing` is true.
#[must_use]
pub(super) fn select_portfolio(
    scored: &[ScoredCandidate],
    policy: &GrowthIntelligencePolicy,
    pending_measurement_count: u32,
    workspace_id: WorkspaceId,
    experimental_keys: &std::collections::HashSet<String>,
    sizing_multiplier: f64,
) -> PortfolioSelection {
    let candidates: Vec<PortfolioCandidate> = scored
        .iter()
        .map(|scored| {
            let (c, p, efe, stats) = (
                &scored.candidate,
                &scored.prediction,
                scored.efe_score,
                &scored.treatment_stats,
            );
            // P1-e: audience_key is target-only, not template+target.
            // Two different templates hitting the same community share
            // an audience_key, so the overlap penalty applies correctly.
            let audience_key = audience_key_for(c, workspace_id);
            // Construct the canonical DecisionValue from treatment-aware
            // stats. This is the single source of truth for all value
            // semantics — the optimizer reads decision_value.total()
            // and applies portfolio interactions to compute marginal
            // value.
            let decision_mode = if stats.use_treatment_effect {
                DecisionMode::Exploit
            } else {
                DecisionMode::Explore
            };
            let decision_value = DecisionValue::from_stats(
                stats,
                template_cost(policy, &p.template_id),
                decision_mode,
            );
            PortfolioCandidate {
                opportunity_id: OpportunityId {
                    template_id: p.template_id.clone(),
                    target: c.decision_key.clone(),
                    action: OpportunityAction::from_template(&p.template_id),
                    context_hash: context_hash(&p.context),
                },
                // EFE is a candidate-generation signal, NOT an economic
                // value. The optimizer ignores generation_signal for
                // ranking — DecisionValue.total() is the sole authority.
                // The exploration trail. Both halves were hard-coded 0.0
                // with a note saying they would be wired later, which left
                // every candidate claiming it had learned nothing and been
                // nowhere — so no exploration decision could be explained
                // after the fact. They are carried from where EFE computes
                // them. The optimizer still ignores all three for ranking.
                generation_signal: Some(EfeSignal {
                    information_gain: scored.information_gain,
                    novelty: scored.novelty,
                    efe_score: efe,
                }),
                audience_key,
                source_context: c.decision_kind.to_string(),
                action_key: c.decision_key.clone(),
                // P0-2: is_experimental is true for treatment-assigned
                // candidates from active experiments. These candidates
                // can use the experimental_dispatch_budget (additional
                // slots beyond max_dispatches) when the VOI justifies it.
                is_experimental: experimental_keys.contains(&c.decision_key),
                decision_value,
            }
        })
        .collect();
    // Compute WAIT candidate value. Every term is in expected Y30 fans.
    // The WAIT candidate's opportunity_cost = -best_action_y30, and
    // wait_total = VOI + fatigue + option - best_action_y30.
    // WAIT wins when wait_total > 0 (net positive utility).
    let best_y30 = candidates
        .iter()
        .map(|c| c.decision_value.total())
        .fold(0.0_f64, f64::max);
    let avg_treatment_std = {
        let stds: Vec<f64> = candidates
            .iter()
            .filter(|c| c.decision_value.uncertainty > 0.0)
            .map(|c| c.decision_value.uncertainty)
            .collect();
        if stds.is_empty() {
            0.0
        } else {
            stds.iter().sum::<f64>() / stds.len() as f64
        }
    };
    // Phase 1: VOI uses the pending measurement count passed by the
    // caller. Fatigue recovery is 0.0 (not yet computed pre-portfolio).
    // The WAIT candidate competes via opportunity cost + VOI: if the
    // best action has low expected Y30 and there are pending measurements
    // whose outcomes could inform the decision, WAIT can win.
    let wait =
        WaitCandidateValue::compute(best_y30, pending_measurement_count, avg_treatment_std, 0.0);
    // P0-2: Wire the experimental dispatch budget from the policy into the
    // optimizer config. This allows additional treatment dispatches beyond
    // max_dispatches when the candidate is part of an active experiment.
    //
    // Metacognition sizing_multiplier scales max_dispatches: when the brain
    // is Initializing or Regressing, it acts cautiously (fewer dispatches).
    // When Improving, it gets full budget. This is a constraint, not a value
    // term — it does not enter DecisionValue.total().
    let base_max_dispatches = PortfolioConfig::default().max_dispatches;
    let scaled_max_dispatches = ((f64::from(base_max_dispatches) * sizing_multiplier)
        .round()
        .max(1.0) as u32)
        .max(1);
    let config = PortfolioConfig {
        max_dispatches: scaled_max_dispatches,
        experimental_dispatch_budget: policy.experimental_dispatch_budget,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer { config };
    optimizer.select_with_wait(candidates, wait)
}

/// Extracts the selected decision keys from a portfolio selection for
/// fast lookup during the dispatch loop.
#[must_use]
pub(super) fn selected_keys(selection: &PortfolioSelection) -> HashSet<String> {
    selection
        .selected
        .iter()
        .map(|c| c.opportunity_id.target.clone())
        .collect()
}

/// The decision-time record for each selected candidate, keyed by decision key.
///
/// `DecisionValue` is computed per cycle and dropped. The prediction and the
/// world-model snapshot are durable, so a later reader can re-derive what the
/// brain *would* decide — against posteriors that have since moved. That is a
/// different question from what it decided, and the two look identical in a
/// report.
///
/// This is the smallest block that closes the difference. It rides in the
/// decision's existing `input_snapshot` jsonb, so there is no schema change and
/// `/v1/admin/autopilot/decisions/{id}/evidence` returns it already.
///
/// Three groups, kept separate because conflating them is the failure mode:
///
/// - **economic** — what the candidate was worth, intrinsic and marginal, and
///   every adjustment between them.
/// - **epistemic** — which estimator produced the number and how much it was
///   standing on. Never combined into the economics.
/// - **identity** — enough to know which code and which policy produced this.
///   Not a reproducibility guarantee: the posteriors are not snapshotted, and
///   claiming otherwise would be the exact lie this block exists to prevent.
#[must_use]
pub(super) fn decision_provenance(
    selection: &PortfolioSelection,
    policy_version: i64,
    belief: &crate::autopilot::BeliefStateOrigin,
) -> HashMap<String, serde_json::Value> {
    selection
        .selected
        .iter()
        .map(|candidate| {
            let value = &candidate.decision_value;
            let adjustments = selection
                .marginal_adjustments
                .get(&candidate.opportunity_id.to_string());
            let record = serde_json::json!({
                "economic": {
                    "intrinsic_y30": value.total(),
                    "pragmatic_value": value.pragmatic_value,
                    "risk_penalty": value.risk_penalty,
                    "opportunity_cost": value.opportunity_cost,
                    "resource_cost_units": value.resource_cost.units,
                    "adjustments": adjustments,
                },
                "epistemic": {
                    "estimation_regime": value.estimation_regime.as_str(),
                    "evidence_quality": value.evidence_quality.as_str(),
                    "sample_size": value.sample_size,
                    "uncertainty": value.uncertainty,
                    "uses_y30": value.uses_y30,
                    "bridge_confidence": value.bridge_confidence,
                    "bridge_is_reliable": value.bridge_is_reliable,
                },
                "policy": {
                    "decision_mode": value.decision_mode,
                    "is_experimental": candidate.is_experimental,
                    // A mutable row counter, kept as a label. The identity is
                    // the hash beside it — see `policy_identity`.
                    "policy_version": policy_version,
                },
                // Why the winner won, in the only terms that answer it: what
                // else was in the pool and what happened to each.
                //
                // `PortfolioSelection.rejected` lived for the length of the
                // cycle and was dropped, so the honest answer to "why did this
                // candidate win" was "because it was selected" — which is not
                // an answer for an optimizer whose decision *is* a comparison.
                "competition": {
                    "considered": selection.selected.len() + selection.rejected.len(),
                    "alternatives": selection
                        .rejected
                        .iter()
                        .map(|rejection| {
                            serde_json::json!({
                                "opportunity_key": rejection.opportunity_key,
                                "reason": rejection.reason,
                                "intrinsic_y30": rejection.intrinsic_y30,
                            })
                        })
                        .collect::<Vec<_>>(),
                },
                "identity": {
                    "optimizer": "submodular_greedy_marginal_v1",
                    "brain_version": env!("CARGO_PKG_VERSION"),
                    // Which beliefs produced the estimate above. The number is
                    // durable; without this the belief that generated it was
                    // not nameable, and re-deriving it later answers "what
                    // would the brain predict now".
                    "belief_state": belief,
                },
            });
            (candidate.opportunity_id.target.clone(), record)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::model::{ActionSubject, AutopilotActionPayload, AutopilotContext};
    use crowdrelay_brain::AgentTier;
    use crowdrelay_domain::autonomy::{Confidence, PolicyDisposition};

    fn candidate(decision_key: &str) -> DecisionCandidate {
        DecisionCandidate {
            context: AutopilotContext::GrowthIntelligence,
            subject: ActionSubject::Workspace(WorkspaceId::from_uuid(uuid::Uuid::nil())),
            decision_kind: "request_agent_run",
            confidence: Confidence::MAX,
            disposition: PolicyDisposition::AutoExecute,
            reason: "fixture",
            input_snapshot: serde_json::json!({}),
            policy_snapshot: serde_json::json!({}),
            action: AutopilotActionPayload::RequestAgentRun {
                template_id: "fixture".to_owned(),
                prompt: String::new(),
                priority: 1,
                tier: AgentTier::Basic,
            },
            decision_key: decision_key.to_owned(),
            action_idempotency_key: decision_key.to_owned(),
        }
    }

    /// The policy identity is content, and content ordering does not change it.
    ///
    /// `policy_version` is a counter on a mutable row with no history table, so
    /// the identity beside it has to be derived from the policy itself. Three
    /// properties make that identity usable, and the third is the one that is
    /// assumed rather than obvious: `serde_json` stores object keys in a
    /// `BTreeMap`, so two snapshots built with their fields in different orders
    /// serialize identically. That is a property of the dependency, not of this
    /// code, so it is pinned here rather than trusted — if it ever changes, two
    /// semantically identical policies would hash differently and every
    /// grouping built on the identity would silently fragment.
    #[test]
    fn the_policy_identity_is_content_and_survives_field_ordering() {
        use crate::autopilot::evaluate::policy_content_identity;

        let policy = serde_json::json!({
            "version": 7,
            "enabled": true,
            "autonomy_level": "bounded_auto",
            "max_actions_24h": 12,
        });

        // 1. Deterministic: the same policy hashes the same way twice.
        assert_eq!(
            policy_content_identity(&policy),
            policy_content_identity(&policy),
            "an identity that changes between calls identifies nothing"
        );

        // 2. Ordering-insensitive: the same policy written in a different
        //    order is the same policy.
        let reordered = serde_json::json!({
            "max_actions_24h": 12,
            "autonomy_level": "bounded_auto",
            "enabled": true,
            "version": 7,
        });
        assert_eq!(
            policy_content_identity(&policy),
            policy_content_identity(&reordered),
            "field order is not a material difference; two identical policies \
             must not hash apart"
        );

        // 3. Sensitive to material change — including the one that matters
        //    most, since it is the authority level.
        let widened = serde_json::json!({
            "version": 7,
            "enabled": true,
            "autonomy_level": "full_auto",
            "max_actions_24h": 12,
        });
        assert_ne!(
            policy_content_identity(&policy),
            policy_content_identity(&widened),
            "a different authority level is a different policy, even at the \
             same version — which is exactly the case the counter cannot see"
        );

        // And the shape is a hash, not a copy of the policy.
        let identity = policy_content_identity(&policy);
        assert!(
            identity.starts_with("sha256:") && identity.len() == 39,
            "expected a truncated sha256 identity, got {identity}"
        );
    }

    /// A historical decision explains why the winner beat the losers.
    ///
    /// A portfolio decision *is* a comparison, and only the winner used to
    /// survive it: `PortfolioSelection.rejected` lived for the length of the
    /// cycle and was dropped. The honest answer to "why did this candidate
    /// win" was "because it was selected", which is not an answer.
    ///
    /// Three candidates, one dispatch slot, so the pool produces a winner and
    /// two losers. Everything asserted here is read back out of the record
    /// attached to the decision — no optimizer is re-run, no configuration is
    /// consulted.
    #[test]
    fn a_decision_records_the_alternatives_and_why_each_one_lost() {
        use crowdrelay_brain::{
            DecisionMode, DecisionValue, EstimationRegime, OpportunityAction, OpportunityId,
            PortfolioCandidate, PortfolioOptimizer, ResourceCost,
        };

        let context = crowdrelay_brain::DispatchContext::default();
        let valued = |fans: f64| {
            let mut value = DecisionValue::from_stats(
                &crowdrelay_brain::CausalModel::new()
                    .predict_stats_with_treatment("fixture", &context),
                ResourceCost::configured(1.0),
                DecisionMode::Exploit,
            );
            value.estimation_regime = EstimationRegime::OutcomeModel;
            value.pragmatic_value = fans;
            value.expected_incremental_y30 = fans;
            value
        };
        let entrant = |name: &str, fans: f64, audience: &str| PortfolioCandidate {
            opportunity_id: OpportunityId {
                template_id: "fixture".to_owned(),
                target: format!("decision:{name}"),
                action: OpportunityAction::Post,
                context_hash: "ctx".to_owned(),
            },
            audience_key: audience.to_owned(),
            source_context: "GrowthIntelligence".to_owned(),
            action_key: format!("decision:{name}"),
            generation_signal: None,
            is_experimental: false,
            decision_value: valued(fans),
        };

        // One slot. A wins; B and C are left over.
        let selection = PortfolioOptimizer::new(PortfolioConfig {
            max_dispatches: 1,
            ..PortfolioConfig::default()
        })
        .select(vec![
            entrant("a", 12.0, "audience-a"),
            entrant("b", 8.0, "audience-b"),
            entrant("c", 3.0, "audience-c"),
        ]);
        assert_eq!(
            selection.selected.len(),
            1,
            "the fixture must produce one winner"
        );

        let belief = crate::autopilot::BeliefStateOrigin::Checkpoint {
            checkpoint_content_hash: "sha256:feedfacefeedfacefeedfacefeedface".to_owned(),
            checkpoint_updated_at: time::OffsetDateTime::UNIX_EPOCH,
            delta_evidence: 4,
        };
        let provenance = decision_provenance(&selection, 7, &belief);
        let mut subject = candidate("decision:a");
        crate::autopilot::evaluate::attach_decision_provenance(
            &mut subject,
            &provenance,
            &serde_json::json!({}),
        );

        // Everything below is read from the decision, not recomputed.
        let record = subject
            .input_snapshot
            .get("decision_value")
            .expect("decision-time record");
        let competition = record.get("competition").expect("competition block");

        assert_eq!(
            competition["considered"], 3,
            "the record must say how large the pool was; a winner with no \
             recorded field is indistinguishable from a winner that ran alone"
        );

        let alternatives = competition["alternatives"]
            .as_array()
            .expect("alternatives array");
        assert_eq!(alternatives.len(), 2, "both losers must survive");

        for alternative in alternatives {
            let key = alternative["opportunity_key"].as_str().expect("key");
            assert!(
                !key.contains("decision:a"),
                "the winner must not appear among the alternatives"
            );
            assert!(
                alternative.get("reason").is_some(),
                "{key} lost without a recorded reason"
            );
            // The reason alone does not explain a loss: the same reason on a
            // candidate worth 8.0 and on one worth 3.0 are different stories.
            let intrinsic = alternative["intrinsic_y30"].as_f64().expect("intrinsic");
            assert!(
                intrinsic > 0.0,
                "{key} must record what it was worth, got {intrinsic}"
            );
        }

        // The winner's own economics, and the inputs that produced them.
        let economic = record.get("economic").expect("economic block");
        let adjustments = &economic["adjustments"];
        assert!(
            adjustments["audience_count"].as_u64().is_some()
                && adjustments["overlap_penalty"].as_f64().is_some()
                && adjustments["fatigue_decay"].as_f64().is_some()
                && adjustments["bridge_factor"].as_f64().is_some(),
            "the adjustment inputs must travel with the deltas — the audience \
             count vanishes with the cycle and the coefficients are config a \
             reader would otherwise take from a since-edited row"
        );

        // Policy identity: a content hash, not the mutable row counter.
        let policy = record.get("policy").expect("policy block");
        let identity = policy["policy_identity"].as_str().expect("policy identity");
        assert!(
            identity.starts_with("sha256:") && identity.len() > 16,
            "the policy identity must be a content hash, got {identity}"
        );
        assert_eq!(
            policy["policy_version"], 7,
            "the counter stays as a label beside it"
        );
    }

    /// The decision-time record reaches the decision, and keeps its categories
    /// apart.
    ///
    /// `DecisionValue` is computed per cycle and dropped. Everything durable —
    /// the prediction, the world-model snapshot, the evidence row — describes
    /// what the brain *saw*, not what it *concluded*, so a later reader could
    /// only re-derive a conclusion against posteriors that had moved. That
    /// reconstruction answers "what would the brain decide now" and is
    /// indistinguishable in a report from "what did it decide".
    ///
    /// Two things are asserted, and the second is the one that decays. The
    /// record must be present and reconstruct the marginal; and economic,
    /// epistemic and policy facts must stay in separate objects. The failure
    /// this prevents is someone flattening them into one bag of numbers, after
    /// which `uncertainty` sits beside `intrinsic_y30` looking like a term.
    #[test]
    fn the_decision_time_record_is_attached_and_keeps_its_categories_apart() {
        use crowdrelay_brain::{
            DecisionMode, DecisionValue, EstimationRegime, EvidenceQuality, OpportunityAction,
            OpportunityId, PortfolioCandidate, PortfolioOptimizer, ResourceCost,
        };

        let context = crowdrelay_brain::DispatchContext::default();
        let mut decision_value = DecisionValue::from_stats(
            &crowdrelay_brain::CausalModel::new().predict_stats_with_treatment("fixture", &context),
            ResourceCost::configured(1.0),
            DecisionMode::Exploit,
        );
        decision_value.estimation_regime = EstimationRegime::Y14Bridged;
        decision_value.evidence_quality = EvidenceQuality::RandomizedHoldout;
        decision_value.bridge_is_reliable = false;
        decision_value.pragmatic_value = 10.0;
        decision_value.expected_incremental_y30 = 10.0;

        let decision_key = "decision:growth-intelligence:v7:fixture:target:0";
        let pool = vec![PortfolioCandidate {
            opportunity_id: OpportunityId {
                template_id: "fixture".to_owned(),
                target: decision_key.to_owned(),
                action: OpportunityAction::Post,
                context_hash: "ctx".to_owned(),
            },
            audience_key: "audience".to_owned(),
            source_context: "GrowthIntelligence".to_owned(),
            action_key: decision_key.to_owned(),
            generation_signal: None,
            is_experimental: false,
            decision_value,
        }];

        let selection = PortfolioOptimizer::new(PortfolioConfig::default()).select(pool);
        let belief = crate::autopilot::BeliefStateOrigin::Checkpoint {
            checkpoint_content_hash: "sha256:feedfacefeedfacefeedfacefeedface".to_owned(),
            checkpoint_updated_at: time::OffsetDateTime::UNIX_EPOCH,
            delta_evidence: 4,
        };
        let provenance = decision_provenance(&selection, 7, &belief);

        let mut subject = candidate(decision_key);
        crate::autopilot::evaluate::attach_decision_provenance(
            &mut subject,
            &provenance,
            &serde_json::json!({}),
        );

        let record = subject
            .input_snapshot
            .get("decision_value")
            .expect("the decision must carry its own decision-time record");

        // Economic: the breakdown reconstructs the marginal the optimizer used.
        let economic = record.get("economic").expect("economic block");
        let intrinsic = economic["intrinsic_y30"].as_f64().expect("intrinsic");
        let adjustments = &economic["adjustments"];
        let marginal = adjustments["marginal_y30"].as_f64().expect("marginal");
        let summed = intrinsic
            + adjustments["overlap_adjustment"].as_f64().expect("overlap")
            + adjustments["fatigue_adjustment"].as_f64().expect("fatigue")
            + adjustments["bridge_adjustment"].as_f64().expect("bridge");
        assert!(
            (summed - marginal).abs() < 1e-9,
            "the persisted breakdown must reach the marginal: {summed} vs {marginal}"
        );
        assert!(
            adjustments["bridge_adjustment"].as_f64().expect("bridge") < 0.0,
            "an uncalibrated Y14Bridged candidate was docked, and the record \
             must say so rather than showing an unexplained gap"
        );

        // Epistemic: which estimator, standing on how much. Not economics.
        let epistemic = record.get("epistemic").expect("epistemic block");
        assert_eq!(epistemic["estimation_regime"], "y14_bridged");
        assert_eq!(epistemic["evidence_quality"], "randomized_holdout");
        assert_eq!(epistemic["bridge_is_reliable"], false);

        // Policy: what constrained the decision.
        let policy = record.get("policy").expect("policy block");
        assert_eq!(policy["policy_version"], 7);
        assert_eq!(policy["is_experimental"], false);

        // Identity: which code produced it.
        let identity = record.get("identity").expect("identity block");
        assert_eq!(identity["optimizer"], "submodular_greedy_marginal_v1");

        // The categories must not be flattened into one another.
        assert!(
            economic.get("uncertainty").is_none() && economic.get("estimation_regime").is_none(),
            "epistemic facts must not appear in the economic block, where they \
             read as terms"
        );
        assert!(
            epistemic.get("intrinsic_y30").is_none(),
            "economic facts must not appear in the epistemic block"
        );
    }

    fn key_for(template: &str) -> String {
        let ws = WorkspaceId::from_uuid(uuid::Uuid::nil());
        audience_key_for(
            &candidate(&format!(
                "decision:growth-intelligence:v1:{template}:target:0"
            )),
            ws,
        )
    }

    #[test]
    fn everything_that_posts_to_the_band_shares_one_audience() {
        // The overlap penalty is what stops three posts to the same people on
        // the same day counting as three separate reaches.
        let expected = key_for("social-post");
        for template in [
            "press-pitch",
            "telegram-poster",
            "discord-poster",
            "signal-inviter",
        ] {
            assert_eq!(key_for(template), expected, "{template}");
        }
        assert!(expected.starts_with("workspace:"));
    }

    #[test]
    fn a_scan_does_not_share_an_audience_with_a_post() {
        // Scanners reached nobody but carried the band's audience key, so one
        // selected scan cut every posting template behind it by 37%, then
        // 68%, then 93%, while community candidates kept full value.
        let post = key_for("social-post");
        for scanner in [
            "reddit-scanner",
            "telegram-scanner",
            "metal-archives-scanner",
            "bandcamp-scanner",
            "growth-strategist",
        ] {
            assert_ne!(
                key_for(scanner),
                post,
                "{scanner} must not fatigue the band"
            );
        }
    }

    #[test]
    fn two_scanners_do_not_fatigue_each_other_either() {
        assert_ne!(key_for("reddit-scanner"), key_for("telegram-scanner"));
    }

    #[test]
    fn each_community_is_its_own_audience() {
        let ws = WorkspaceId::from_uuid(uuid::Uuid::nil());
        let a = audience_key_for(
            &candidate("decision:growth-intelligence:v1:community-engager:aaa:0"),
            ws,
        );
        let b = audience_key_for(
            &candidate("decision:growth-intelligence:v1:community-engager:bbb:0"),
            ws,
        );
        assert_eq!(a, "community:aaa");
        assert_ne!(a, b);
    }

    #[test]
    fn a_decision_key_from_another_context_is_its_own_target() {
        let ws = WorkspaceId::from_uuid(uuid::Uuid::nil());
        let key = audience_key_for(&candidate("decision:plays:v1:something:else"), ws);
        assert!(key.starts_with("target:"), "{key}");
    }
}
