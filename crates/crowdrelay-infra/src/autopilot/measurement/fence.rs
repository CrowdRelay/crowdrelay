use super::super::*;
pub(super) async fn lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<(), RepositoryError> {
    let owner = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM autopilot_measurements WHERE workspace_id=$1 AND id=$2 AND status='processing' AND attempt_count=$3 FOR UPDATE")
        .bind(workspace.into_uuid()).bind(measurement.id.into_uuid())
        .bind(i32::try_from(measurement.attempt_number).map_err(|_| RepositoryError::Unexpected)?)
        .fetch_optional(&mut **tx).await.map_err(map_sqlx)?;
    owner.ok_or(RepositoryError::Conflict)?;
    Ok(())
}
