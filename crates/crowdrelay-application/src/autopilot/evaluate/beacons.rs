//! Beacon-specific candidate construction kept out of the core evaluation loop.

use super::*;

pub(super) fn beacon_discovery_candidate(
    snapshot: BeaconDiscoverySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Beacon(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let BeaconDiscoveryDecision::Request {
        target_count,
        confidence,
    } = evaluate_beacon_discovery(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let last_discovery = snapshot
        .last_discovery_at
        .map_or(0, OffsetDateTime::unix_timestamp);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind: "discover_local_beacons",
        confidence,
        disposition,
        reason: "upcoming show needs more verified local Beacons before the outreach window",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestBeaconDiscovery {
            event_id: snapshot.event_id,
            target_count,
        },
        decision_key: format!(
            "decision:beacon-discovery:v{}:{}:{}:{}",
            policy.version, snapshot.event_id, snapshot.known_local_beacons, last_discovery
        ),
        action_idempotency_key: format!(
            "action:beacon-discovery:{}:{}:{}",
            snapshot.event_id, snapshot.known_local_beacons, last_discovery
        ),
    }))
}

pub(super) fn beacon_candidate(
    snapshot: BeaconCampaignSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Beacon(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let BeaconDecision::Request { phase, confidence } =
        evaluate_beacon_campaign(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let template_key = match phase {
        BeaconOutreachPhase::Initial => "beacon.local_story.v1",
        BeaconOutreachPhase::CollaborationFollowUp => "beacon.collaboration.v1",
        BeaconOutreachPhase::LocalPush => "beacon.local_push.v1",
        BeaconOutreachPhase::PostShowThanks => "beacon.thanks.v1",
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Beacon(snapshot.beacon_id),
        decision_kind: "amplify_local_signal",
        confidence,
        disposition,
        reason: "verified local Beacon is relevant and the show-specific contact window is due",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestBeaconOutreach {
            beacon_id: snapshot.beacon_id,
            event_id: snapshot.event_id,
            beacon_version: snapshot.beacon_version,
            phase,
            template_key: template_key.to_owned(),
        },
        decision_key: format!(
            "decision:beacon:v{}:{}:{}:bv{}:{:?}:{}",
            policy.version,
            snapshot.beacon_id,
            snapshot.event_id,
            snapshot.beacon_version,
            phase,
            snapshot.followup_count,
        ),
        action_idempotency_key: format!(
            "action:beacon:{}:{}:{:?}:{}",
            snapshot.beacon_id, snapshot.event_id, phase, snapshot.followup_count
        ),
    }))
}
