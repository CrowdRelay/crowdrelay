//! Execution boundary for content-artifact requests.
//!
//! The queue entry asks an external executor to produce a release artifact;
//! the only first-party write here is the emitted intent. What makes it an
//! execution boundary rather than a passthrough is the re-check: the plan's
//! switches are read again against the locked source row, because a flag
//! flipped after the request queued is a newer decision than the queue
//! entry — the same rule the release milestone ladder applies.

use crowdrelay_domain::release_autopilot::ReleaseTier;
use serde_json::json;

use super::*;

pub(in crate::autopilot) async fn execute_content_artifact(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    source_id: crowdrelay_domain::ContentSourceId,
    artifact: crowdrelay_domain::content_supply::ContentArtifactKind,
    template_key: &str,
) -> Result<(), RepositoryError> {
    let source = load_content_source_for_execution(transaction, workspace_id, source_id).await?;
    // Scoped to release sources — the vocabulary is release-plan owned, and
    // the evaluator reads it under the same scope.
    if source.0 == "release" {
        let flag = |key: &str| source.2.get(key).and_then(serde_json::Value::as_bool);
        let tier = source
            .2
            .get("tier")
            .and_then(serde_json::Value::as_str)
            .and_then(ReleaseTier::parse);
        if !crowdrelay_domain::content_supply::content_artifact_owed(
            artifact,
            flag("communication_enabled"),
            flag("press_enabled"),
            tier,
        ) {
            return Err(RepositoryError::Conflict);
        }
    }
    crate::autopilot::emit_external_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.content.artifact_requested",
        json!({
            "action_id": action_id,
            "source_id": source_id,
            "source_kind": source.0,
            "source_title": source.1,
            "source_metadata": source.2,
            "artifact": artifact,
            "template_key": template_key,
        }),
    )
    .await
}
