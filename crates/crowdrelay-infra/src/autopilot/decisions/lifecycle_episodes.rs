// Preserve old requests when switching from touch-based keys to episode keys.
// A completed/failed/declined/unknown request must not be replayed under a new
// identity. Recovery, when justified, belongs to that original action.

async fn lifecycle_episode_already_answered(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    candidate: &DecisionCandidate,
) -> Result<bool, RepositoryError> {
    let AutopilotActionPayload::RequestFanLifecycleMessage {
        fan_id,
        template_key,
        ..
    } = &candidate.action
    else {
        return Ok(false);
    };
    let Some(value) = candidate.input_snapshot.get("lifecycle_episode") else {
        return Ok(false);
    };
    let episode: crowdrelay_domain::lifecycle_episode::LifecycleEpisode =
        serde_json::from_value(value.clone()).map_err(|_| RepositoryError::Unexpected)?;
    sqlx::query_scalar(LIFECYCLE_EPISODE_ANSWERED_SQL)
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .bind(template_key)
        .bind(&candidate.action_idempotency_key)
        .bind(&episode.key)
        .bind(episode.since)
        .bind(episode.ticket_count.map(i64::from))
        .bind(episode.event_slug)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)
}

const LIFECYCLE_EPISODE_ANSWERED_SQL: &str = r#"
SELECT EXISTS (
    SELECT 1 FROM autopilot_actions action
    LEFT JOIN autopilot_decisions decision
      ON decision.workspace_id=action.workspace_id AND decision.id=action.decision_id
    WHERE action.workspace_id=$1 AND action.subject_id=$2
      AND action.action_kind='fan.lifecycle.message.request'
      AND (action.payload->>'template_key'=$3
        OR ($3 IN ('crowdrelay.fan.welcome.v1','crowdrelay.fan.welcome.v2')
            AND action.payload->>'template_key' IN ('crowdrelay.fan.welcome.v1','crowdrelay.fan.welcome.v2')))
      -- The current family's own uniqueness/lapse bound already handles it.
      AND action.idempotency_key<>$4
      AND NOT starts_with(action.idempotency_key,$4 || ':lapsed:')
      AND (
        decision.input_snapshot->'lifecycle_episode'->>'key'=$5
        OR (decision.input_snapshot->'lifecycle_episode' IS NULL AND (
          ($6::timestamptz IS NULL AND $7::bigint IS NULL)
          OR ($7::bigint IS NOT NULL AND decision.input_snapshot->>'paid_ticket_count'=$7::text)
          OR ($6::timestamptz IS NOT NULL AND action.created_at >= $6
              AND ($8::text IS NULL OR action.payload->'show'->>'event_slug'=$8))
        ))
      )
)
"#;
