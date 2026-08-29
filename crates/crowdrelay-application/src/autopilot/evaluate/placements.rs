//! Turning a due verification into a durable decision.
//!
//! Separated from the cycle for the same reason the play candidate is: the
//! judgement about whether a placement needs reading belongs to
//! `crowdrelay_domain::playlist_placement`, and what lives here is only the
//! translation into the shape the rest of the system already understands.

use super::*;

/// Walks every pending playlist placement and either settles it or queues a
/// verification candidate. Extracted from the main evaluator to keep it
/// under the modularity limit.
pub(super) async fn follow_through_placements<R>(
    evaluator: &EvaluateAutopilot<'_, R>,
    policy: &AutopilotPolicy,
    limits: &mut CycleLimits<'_>,
    report: &mut AutopilotCycleReport,
    now: OffsetDateTime,
) -> Result<(), AutopilotError>
where
    R: AutopilotDecisionRepository,
{
    let placement_policy = PlacementPolicy::default();
    for entry in evaluator
        .repository
        .load_playlist_placements(evaluator.workspace_id, now)
        .await?
    {
        match evaluate_placement(entry.placement, placement_policy, now) {
            PlacementDecision::Hold => {}
            PlacementDecision::Settle { state } => {
                evaluator
                    .repository
                    .settle_playlist_placement(
                        evaluator.workspace_id,
                        PlacementSettlement {
                            opportunity_id: entry.placement.opportunity_id,
                            state,
                        },
                        now,
                    )
                    .await?;
                report.placements_settled = report.placements_settled.saturating_add(1);
            }
            PlacementDecision::Verify { checkpoint } => {
                let candidate = placement_candidate(&entry, policy, placement_policy, checkpoint)?;
                evaluator.persist(&candidate, limits, report).await?;
            }
        }
    }
    Ok(())
}

/// One public read of one claimed placement, as a durable candidate.
pub(super) fn placement_candidate(
    entry: &PlaylistPlacementSnapshot,
    policy: &AutopilotPolicy,
    placement_policy: PlacementPolicy,
    checkpoint: u8,
) -> Result<DecisionCandidate, serde_json::Error> {
    Ok(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::OutreachOpportunity(entry.placement.opportunity_id),
        decision_kind: "verify_playlist_placement",
        // Reading a public playlist is not a judgement call, and a low
        // confidence here would park the one action whose whole purpose is to
        // be independent of the claim it is checking.
        confidence: Confidence::MAX,
        disposition: disposition(
            policy.autonomy_level,
            Confidence::MAX,
            policy.minimum_confidence,
        ),
        reason: "a claimed placement counts toward nothing until a public read confirms it, and \
                 confirmations are re-read because adding a track for a screenshot is a known \
                 pattern",
        input_snapshot: serde_json::to_value(entry.placement)?,
        policy_snapshot: serde_json::to_value(placement_policy)?,
        action: AutopilotActionPayload::VerifyPlaylistPlacement {
            opportunity_id: entry.placement.opportunity_id,
            playlist_external_id: entry.playlist_external_id.clone(),
            track_external_id: entry.track_external_id.clone(),
            checkpoint,
        },
        // The checkpoint is in the key. Without it the second read of the same
        // placement is deduplicated against the first and the re-check never
        // happens, which is precisely the check a scammer needs to miss.
        decision_key: format!(
            "decision:placement:v{}:{}:{}",
            policy.version, entry.placement.opportunity_id, checkpoint
        ),
        action_idempotency_key: format!(
            "action:placement:{}:{}",
            entry.placement.opportunity_id, checkpoint
        ),
    })
}
