//! Outreach wave sizing, split from `evaluate.rs`.
//!
//! Opening a wave is where the deliverability ceiling bites: the operator's
//! budget is a ceiling, not a target, and a halted sender gets zero.

//! The one decision that keeps the pitcher from looping over an empty table.
//!
//! Every outreach capability the executor advertises reads from
//! `viryaos_outreach_targets`. Discovery fills that table, but discovery has
//! only ever been an inbound endpoint: something outside had to decide to run
//! it. That made zero targets a stable state rather than a problem the agent
//! could notice, which is the difference between a growth engine and a very
//! well-tested set of rules about growth.

use super::*;

pub(super) fn outreach_supply_candidate(
    snapshot: &OutreachSupplySnapshot,
    policy: &AutopilotPolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::OutreachSupply(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let OutreachSupplyDecision::Request {
        requested_candidates,
        confidence,
    } = evaluate_outreach_supply(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    // The keys carry the sweep the request is a reaction to, not the clock.
    // Two cycles that see the same starved pipeline are the same decision, and
    // re-asking would spend an API call to learn what the last one already did.
    let last_sweep = snapshot
        .last_sweep_requested_at
        .map_or(0, OffsetDateTime::unix_timestamp);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Workspace(workspace_id),
        decision_kind: "replenish_outreach_supply",
        confidence,
        disposition,
        reason: "the pitcher has fewer confirmed submission routes than the policy floor",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestOutreachDiscovery {
            requested_candidates,
        },
        decision_key: format!(
            "decision:outreach-supply:v{}:{}:{}:{}",
            policy.version, snapshot.pitchable_targets, snapshot.admitted_candidates, last_sweep
        ),
        action_idempotency_key: format!(
            "action:outreach-supply:{}:{}:{}",
            snapshot.pitchable_targets, snapshot.admitted_candidates, last_sweep
        ),
    }))
}

impl<R: AutopilotDecisionRepository> EvaluateAutopilot<'_, R> {
    /// Opens a wave for any anchor that deserves one and has none.
    ///
    /// The capacity is frozen here, against the budget as it stands now. A wave
    /// re-sized every cycle would grow while somebody was reading it, and an
    /// operator looking at a sealed wave should see the budget it was drafted
    /// under rather than today's.
    pub(super) async fn open_outreach_waves(
        &self,
        policy: &AutopilotPolicy,
        report: &mut AutopilotCycleReport,
        now: OffsetDateTime,
    ) -> Result<(), AutopilotError> {
        let AutopilotPolicyConfig::Outreach(outreach_policy) = policy.config else {
            return Ok(());
        };
        let wave_policy = outreach_policy.waves;
        // The same envelope and the same usage the cycle already throttles
        // against. A wave sized from anything else would be sized against a
        // budget that does not exist.
        let (envelope, usage) = self
            .repository
            .load_growth_envelope(self.workspace_id, now)
            .await?;
        // And the operator's budget is a ceiling, not a target. A workspace
        // still earning its sending reputation gets less than the ceiling, and
        // one whose bounce or complaint rate has gone up gets nothing at all —
        // a halt has to be a precondition of sending rather than a line in a
        // digest somebody reads after the reputation is spent.
        let deliverability = self
            .repository
            .load_deliverability_snapshot(self.workspace_id, now)
            .await?;
        let ceiling = ramped_ceiling(deliverability, DeliverabilityPolicy::default(), now)
            .min(envelope.weekly_third_party_touches);
        let remaining = ceiling.saturating_sub(usage.third_party_touches_7d);
        for anchor in self
            .repository
            .load_outreach_wave_anchors(self.workspace_id, now)
            .await?
        {
            if !wave_is_worth_opening(
                anchor.active,
                anchor.hours_until,
                anchor.eligible_targets,
                remaining,
                wave_policy,
            ) {
                continue;
            }
            let capacity = wave_capacity(
                WaveSnapshot {
                    anchor: anchor.anchor,
                    target_kind: anchor.target_kind,
                    state: WaveState::Drafting,
                    opened_at: now,
                    anchor_at: anchor.anchor_at,
                    pitches: 0,
                    eligible_targets: anchor.eligible_targets,
                    third_party_budget_remaining: remaining,
                    anchor_active: anchor.active,
                },
                wave_policy,
            );
            if capacity == 0 {
                continue;
            }
            if self
                .repository
                .open_outreach_wave(
                    self.workspace_id,
                    &OutreachWaveStart {
                        anchor: anchor.anchor,
                        anchor_at: anchor.anchor_at,
                        target_kind: anchor.target_kind,
                        capacity,
                    },
                )
                .await?
            {
                report.waves_opened = report.waves_opened.saturating_add(1);
            }
        }
        Ok(())
    }
}
