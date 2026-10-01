use super::*;

/// Recheck the exact installed-template approval at emission, with locks held
/// through the outbox commit. Revocation and policy changes cannot race a send.
pub(super) async fn check_install_grant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    action: &ClaimedAutopilotAction,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let AutopilotActionPayload::RequestFanLifecycleMessage {
        template_key,
        fan_id,
        ..
    } = &action.payload
    else {
        return Ok(());
    };
    if template_key != "crowdrelay.fan.signal_install_ask.v1" {
        return Ok(());
    }
    let installed = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM signal_installations WHERE workspace_id=$1 AND fan_id=$2)",
    )
    .bind(workspace.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    if installed {
        return Err(RepositoryError::ConflictBecause(
            "signal install ask no longer needed",
        ));
    }
    let standing=sqlx::query_scalar::<_,bool>("SELECT COALESCE(approved_by='operator:standing_grant',false) FROM autopilot_actions WHERE workspace_id=$1 AND id=$2")
        .bind(workspace.into_uuid()).bind(action.id.into_uuid()).fetch_one(&mut **tx).await.map_err(map_sqlx)?;
    if !standing {
        return Ok(());
    }
    let target = action
        .payload
        .standing_approval_target()
        .ok_or(RepositoryError::Conflict)?;
    let live=sqlx::query_scalar::<_,String>("SELECT target_key FROM standing_approvals WHERE workspace_id=$1 AND action_kind=$2 AND target_key=$3 AND action_class='owned_audience' AND revoked_at IS NULL AND expires_at>$4 FOR SHARE")
        .bind(workspace.into_uuid()).bind(action.payload.action_kind()).bind(target).bind(now)
        .fetch_optional(&mut **tx).await.map_err(map_sqlx)?;
    if live.is_none() {
        return Err(RepositoryError::ConflictBecause(
            "signal install standing approval expired or revoked",
        ));
    }
    let permitted=sqlx::query_scalar::<_,bool>("SELECT enabled AND autonomy_level IN ('require_approval','bounded_auto') FROM autopilot_policies WHERE workspace_id=$1 AND context='fan_lifecycle' FOR SHARE")
        .bind(workspace.into_uuid()).fetch_optional(&mut **tx).await.map_err(map_sqlx)?;
    if permitted != Some(true) {
        return Err(RepositoryError::ConflictBecause(
            "fan lifecycle policy no longer permits template approval",
        ));
    }
    Ok(())
}
