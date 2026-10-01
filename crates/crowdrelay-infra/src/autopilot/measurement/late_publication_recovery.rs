//! Reopen only never-observed publication failures; learned evidence is immutable.
use super::super::*;
use super::observation::attributed_fans::{FIRST_TRACKED_POST, FIRST_TRACKED_POST_WITH_TASKS};
pub(super) async fn recover(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, workspace: WorkspaceId,
    with_tasks: bool, now: OffsetDateTime) -> Result<(), RepositoryError> {
    let first_live = if with_tasks { FIRST_TRACKED_POST_WITH_TASKS } else { FIRST_TRACKED_POST }
        .replace("$2", "candidate.action_id");
    let sql = format!(r#"
        WITH candidates AS MATERIALIZED (
            SELECT m.id,m.action_id,m.finished_at FROM autopilot_measurements m
            WHERE m.workspace_id=$1 AND m.status='failed' AND m.last_error_kind=$3
              AND m.measurement_kind IN ('agent_run_fan_growth_3d','incremental_fan_growth_3d',
                  'agent_run_fan_growth_14d','incremental_fan_growth_14d','durable_fan_growth_30d',
                  'content_link_clicks_7d','content_fan_acquisition_7d')
              AND NOT EXISTS(SELECT 1 FROM autopilot_outcomes o WHERE o.workspace_id=$1 AND o.action_id=m.action_id)
              AND NOT EXISTS(SELECT 1 FROM growth_evidence e WHERE e.workspace_id=$1 AND e.action_id=m.action_id
                  AND (e.observed_fans IS NOT NULL OR e.observed_incremental_fans IS NOT NULL
                      OR e.observed_incremental_fans_3d IS NOT NULL OR e.durable_fans_30d IS NOT NULL OR e.observed_metrics <> '{{}}'::jsonb))
            ORDER BY m.publication_recovery_checked_at NULLS FIRST,m.finished_at DESC,m.id
            FOR UPDATE SKIP LOCKED LIMIT 100
        ), inspected AS MATERIALIZED (
            SELECT candidate.id,published.first_live > candidate.finished_at AND published.first_live <= $2 AS revive
            FROM candidates candidate CROSS JOIN LATERAL ({first_live}) published(first_live)
        ), updated AS (
            UPDATE autopilot_measurements m SET publication_recovery_checked_at=$2,
                status=CASE WHEN inspected.revive THEN 'pending' ELSE m.status END,
                attempt_count=CASE WHEN inspected.revive THEN 0 ELSE m.attempt_count END,
                started_at=CASE WHEN inspected.revive THEN NULL ELSE m.started_at END,
                finished_at=CASE WHEN inspected.revive THEN NULL ELSE m.finished_at END,
                due_at=CASE WHEN inspected.revive THEN $2 ELSE m.due_at END,
                available_at=CASE WHEN inspected.revive THEN $2 ELSE m.available_at END,
                last_error_kind=CASE WHEN inspected.revive THEN 'late_publication_recovered' ELSE m.last_error_kind END
            FROM inspected WHERE m.workspace_id=$1 AND m.id=inspected.id RETURNING m.action_id,m.status
        ), revived AS (SELECT action_id FROM updated WHERE status='pending'), evidence_reset AS (
            UPDATE growth_evidence e SET resolved_at=NULL,replayed_3d_at=NULL,replayed_14d_at=NULL,replayed_30d_at=NULL
            WHERE e.workspace_id=$1 AND e.action_id IN (SELECT action_id FROM revived) RETURNING e.action_id
        ) UPDATE dispatch_predictions p SET resolved_at=NULL WHERE p.workspace_id=$1 AND p.action_id IN (SELECT action_id FROM revived)
    "#);
    sqlx::query(&sql).bind(workspace.into_uuid()).bind(now).bind(AutopilotMeasurementKind::NO_TRACKED_LINK)
        .execute(&mut **tx).await.map_err(map_sqlx)?;
    Ok(())
}
