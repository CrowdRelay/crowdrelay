//! Provenance emission framework — the canonical hook at the
//! action/outcome boundary that records fan provenance events.
//!
//! The framework owns:
//! - event validation
//! - idempotency (append-only, no updates)
//! - persistence via `record_fan_provenance_event`
//! - workspace/action linkage
//!
//! Template-specific code owns:
//! - what constitutes exposure/interaction/conversion for this intervention
//! - the attribution method and confidence level
//!
//! PROVENANCE ≠ CAUSALITY. These events establish exposure/attribution
//! evidence. They do NOT automatically establish causal treatment effect.
//! The semantic layers are kept separate:
//!   EXPOSURE → ATTRIBUTION → CAUSAL ESTIMATE
//!
//! P1-f: The framework is intentionally tiny and event-driven. Do not
//! build a giant generalized attribution subsystem here. Each template
//! adapter defines what exposure/conversion means for its intervention.

use crowdrelay_brain::{FanProvenanceEvent, ProvenanceEventKind};
use crowdrelay_domain::WorkspaceId;

use super::{PostgresAutopilotRepository, RepositoryError, experiment_assignments};

/// Emits an Exposure provenance event when a community-engager action
/// completes (post published). The exposure is anonymous (fan_id=None)
/// because we don't know who saw the post. Attribution method is
/// "action_completion" with confidence 1.0 — we know the post was
/// published.
///
/// This is the first link in the provenance chain:
///   Exposure → Interaction → Conversion → Durability
#[allow(dead_code)] // Wired via port trait from application layer
pub(in crate::autopilot) async fn emit_community_engager_exposure(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    action_id: uuid::Uuid,
    community: &str,
    now: time::OffsetDateTime,
) -> Result<(), RepositoryError> {
    let event = FanProvenanceEvent {
        fan_id: None, // Anonymous — we don't know who saw the post
        event_kind: ProvenanceEventKind::Exposure,
        channel: "reddit".to_owned(),
        source_target: Some(community.to_owned()),
        community: Some(community.to_owned()),
        campaign_id: None,
        action_id: Some(action_id),
        attribution_method: "action_completion".to_owned(),
        attribution_confidence: 1.0, // We know the post was published
        occurred_at: now,
    };
    experiment_assignments::record_fan_provenance_event(repo, workspace_id, &event).await
}

/// Emits a Conversion provenance event for a fan that appeared in the
/// community's audience after a community-engager action. Uses
/// `attribution_method="temporal_association"` with `confidence=0.3`
/// when the only evidence is "fan appeared after post in this community."
///
/// This is explicitly NOT fake provenance — the attribution method and
/// confidence make the uncertainty explicit. The measurement system
/// handles this via `attribution_method` and `attribution_confidence`.
#[allow(dead_code)] // Wired via port trait from application layer
pub(in crate::autopilot) async fn emit_community_engager_conversion_temporal(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    fan_id: uuid::Uuid,
    action_id: Option<uuid::Uuid>,
    community: &str,
    occurred_at: time::OffsetDateTime,
) -> Result<(), RepositoryError> {
    let event = FanProvenanceEvent {
        fan_id: Some(fan_id),
        event_kind: ProvenanceEventKind::Conversion,
        channel: "reddit".to_owned(),
        source_target: Some(community.to_owned()),
        community: Some(community.to_owned()),
        campaign_id: None,
        action_id,
        attribution_method: "temporal_association".to_owned(),
        attribution_confidence: 0.3, // Low confidence — temporal only
        occurred_at,
    };
    experiment_assignments::record_fan_provenance_event(repo, workspace_id, &event).await
}

/// Emits a Durability provenance event when a fan is still active 30 days
/// after conversion. This is the final link in the provenance chain.
#[allow(dead_code)] // Wired via port trait from application layer
pub(in crate::autopilot) async fn emit_durability_event(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    fan_id: uuid::Uuid,
    community: &str,
    action_id: Option<uuid::Uuid>,
    occurred_at: time::OffsetDateTime,
) -> Result<(), RepositoryError> {
    let event = FanProvenanceEvent {
        fan_id: Some(fan_id),
        event_kind: ProvenanceEventKind::Durability,
        channel: "reddit".to_owned(),
        source_target: Some(community.to_owned()),
        community: Some(community.to_owned()),
        campaign_id: None,
        action_id,
        attribution_method: "temporal_association".to_owned(),
        attribution_confidence: 0.3,
        occurred_at,
    };
    experiment_assignments::record_fan_provenance_event(repo, workspace_id, &event).await
}
