// Included by snapshots.rs: bounded, platform-aware community selection.

/// Task-level relay failures: the action `succeeded` — it queued the
/// drafting task — but the task itself died. Counted beside the action's
/// own failures so a dead `community-repost` run retries under an attempt
/// key instead of freezing the (source, community) lane as consumed forever.
/// Same shape as `load_failed_drop_surge_tasks`, keyed by target this time.
async fn load_failed_relay_tasks(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<HashMap<Uuid, Vec<RelayLaneFailure>>, RepositoryError> {
    let Some(completion) = task_completion_projection(&repo.pool).await? else {
        return Ok(HashMap::new());
    };
    let sql = r#"
        SELECT action.subject_id,
               split_part(action.idempotency_key, ':', 3)::uuid AS source_id,
               count(*)::bigint,
               max(COALESCE(task.completed_at, task.created_at))
        FROM autopilot_actions action
        JOIN LATERAL (
            SELECT t.status, t.created_at, __TASK_COMPLETED_AT__
            FROM agent_service_tasks t
            WHERE t.workspace_id = action.workspace_id
              AND t.metadata->>'action_id' = action.id::text
            ORDER BY t.created_at DESC, t.id DESC
            LIMIT 1
        ) task ON true
        WHERE action.workspace_id = $1
          AND action.context = 'content_supply'
          AND action.action_kind = 'agent.run.request'
          AND action.subject_kind = 'target_community'
          AND action.status = 'succeeded'
          AND task.status = 'failed'
          AND split_part(action.idempotency_key, ':', 2) = 'relay'
          AND split_part(action.idempotency_key, ':', 4) = 'community'
        GROUP BY action.subject_id, source_id
        "#
    .replace("__TASK_COMPLETED_AT__", completion);
    let rows = sqlx::query_as::<_, (Uuid, Uuid, i64, OffsetDateTime)>(&sql)
        .bind(workspace_id.into_uuid())
        .fetch_all(&repo.pool)
        .await
        .map_err(map_sqlx)?;
    let mut failures: HashMap<Uuid, Vec<RelayLaneFailure>> = HashMap::new();
    for (target, source, count, last_failed_at) in rows {
        failures.entry(target).or_default().push(RelayLaneFailure {
            source_id: ContentSourceId::from_uuid(source),
            failures: u32::try_from(count).unwrap_or(u32::MAX),
            last_failed_at,
        });
    }
    Ok(failures)
}

pub(in crate::autopilot) async fn load_relay_community_targets(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<Vec<CommunityRelayTarget>, RepositoryError> {
    let rows = sqlx::query_as::<_, RelayTargetRow>(
        r#"
        SELECT t.id,
               CASE WHEN COALESCE(NULLIF(t.platform, ''), 'reddit') = 'reddit'
                    THEN t.subreddit ELSE t.display_name END AS community_label,
               t.language, COALESCE(NULLIF(t.platform, ''), 'reddit') AS platform,
               COALESCE(NULLIF(t.community_url, ''), place.url) AS community_url,
               COALESCE(relay_failed.source_ids, ARRAY[]::uuid[]) AS relay_failure_sources,
               COALESCE(relay_failed.counts, ARRAY[]::bigint[]) AS relay_failure_counts,
               COALESCE(relay_failed.last_failed, ARRAY[]::timestamptz[]) AS relay_failure_last,
               room.threads
        FROM agent_outreach_targets t
        LEFT JOIN discovery_places place
          ON place.id = t.place_id AND place.workspace_id = t.workspace_id
        LEFT JOIN discovery_place_rules rules ON rules.place_id = place.id
        LEFT JOIN LATERAL (
            -- A source-bound drafting request consumes a turn before its
            -- outcome becomes a post. Otherwise the same initial targets
            -- monopolize the pool while their task keys dedupe every cycle.
            SELECT MAX(turn.created_at) AS last_draft_at
            FROM (
                SELECT cp.created_at
                FROM community_posts cp
                WHERE cp.workspace_id = t.workspace_id AND cp.target_id = t.id
                  AND cp.status IN ('pending', 'awaiting_manual_post', 'posted', 'rate_limited')
                UNION ALL
                SELECT action.created_at
                FROM autopilot_actions action
                WHERE action.workspace_id = t.workspace_id
                  AND action.subject_id = t.id AND action.subject_kind = 'target_community'
                  AND action.context = 'content_supply' AND action.action_kind = 'agent.run.request'
                  AND action.status IN ('awaiting_approval', 'queued', 'processing', 'succeeded')
                  AND split_part(action.idempotency_key, ':', 2) IN ('relay', 'drop_surge')
                  AND split_part(action.idempotency_key, ':', 4) = 'community'
                  AND split_part(action.idempotency_key, ':', 5) = t.id::text
            ) turn
        ) last ON true
        LEFT JOIN LATERAL (
            -- Dispatches into this community whose action itself failed,
            -- grouped by the source they carried (key element 3). Attempt
            -- keys land here too — element 5 stays the target id, so every
            -- attempt's failure counts toward the same lane.
            SELECT array_agg(source_part ORDER BY source_part) AS source_ids,
                   array_agg(failures ORDER BY source_part) AS counts,
                   array_agg(last_failed ORDER BY source_part) AS last_failed
            FROM (
                SELECT split_part(action.idempotency_key, ':', 3)::uuid AS source_part,
                       count(*)::bigint AS failures,
                       max(COALESCE(action.finished_at, action.updated_at)) AS last_failed
                FROM autopilot_actions action
                WHERE action.workspace_id = t.workspace_id
                  AND action.context = 'content_supply'
                  AND action.action_kind = 'agent.run.request'
                  AND action.status = 'failed'
                  AND split_part(action.idempotency_key, ':', 2) = 'relay'
                  AND split_part(action.idempotency_key, ':', 4) = 'community'
                  AND split_part(action.idempotency_key, ':', 5) = t.id::text
                GROUP BY source_part
            ) per_source
        ) relay_failed ON true
        -- What the room is discussing now: the threads the community sweep read
        -- there (`fan_observations`, kind 'post'), each with its own date and
        -- permalink. The window is `room_reading::READ_MAX_AGE_DAYS`.
        LEFT JOIN LATERAL (
            SELECT COALESCE(jsonb_agg(jsonb_build_object(
                       'title', recent.fact,
                       'url', recent.url,
                       'posted_on', recent.observed_on) ORDER BY recent.observed_on DESC, recent.url), '[]'::jsonb)
                       AS threads
            FROM (
                SELECT DISTINCT ON (fo.url) fo.fact, fo.url, fo.observed_at::date AS observed_on
                FROM fan_observations AS fo
                WHERE fo.workspace_id = t.workspace_id
                  AND fo.place_id = t.place_id
                  AND fo.kind = 'post'
                  AND fo.url IS NOT NULL
                  AND fo.observed_at >= current_date - 14
                  AND fo.observed_at <= current_date
                ORDER BY fo.url, fo.observed_at DESC
            ) AS recent
        ) AS room ON true
        WHERE t.workspace_id = $1
          AND t.target_kind = 'community'
          AND t.screening_verdict = 'admitted'
          AND t.status = 'promoted'
          AND COALESCE(NULLIF(t.platform, ''), 'reddit') IN ('reddit','forum','lemmy','telegram','discord')
          AND COALESCE(rules.self_promo_ratio_percent, 100) > 0
          AND (
              (COALESCE(NULLIF(t.platform, ''), 'reddit') = 'reddit'
               AND NULLIF(btrim(t.subreddit), '') IS NOT NULL
               AND (place.id IS NULL OR (place.status = 'active'
                    AND place.membership_state NOT IN ('rejected', 'not_a_fit'))))
              OR (t.platform <> 'reddit' AND place.status = 'active'
                  AND place.membership_state = 'joined'
                  AND NULLIF(btrim(COALESCE(t.community_url, place.url)), '') IS NOT NULL)
          )
          -- A held Reddit account must not block drafting for a joined forum.
          AND (SELECT count(*) FROM community_posts waiting
               WHERE waiting.workspace_id = t.workspace_id
                 AND waiting.platform = COALESCE(NULLIF(t.platform, ''), 'reddit')
                 AND waiting.status IN ('pending', 'awaiting_manual_post')) < $3
          AND NOT EXISTS (SELECT 1 FROM autopilot_actions drafting
                          WHERE drafting.workspace_id=t.workspace_id AND drafting.subject_id=t.id
                            AND drafting.subject_kind='target_community' AND drafting.context='content_supply'
                            AND drafting.action_kind='agent.run.request'
                            AND drafting.status IN ('awaiting_approval','queued','processing'))
          AND NOT EXISTS (SELECT 1 FROM community_posts waiting
                          WHERE waiting.workspace_id=t.workspace_id AND waiting.target_id=t.id
                            AND waiting.status IN ('pending','awaiting_manual_post','rate_limited'))
        ORDER BY last.last_draft_at ASC NULLS FIRST, t.created_at, t.id
        LIMIT $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(MAX_RELAY_COMMUNITIES_PER_POST)
    .bind(MAX_WAITING_COMMUNITY_DRAFTS)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    let mut task_failures = load_failed_relay_tasks(repo, workspace_id).await?;
    rows.into_iter()
        .map(|row| {
            let mut relay_failures: Vec<RelayLaneFailure> = row
                .relay_failure_sources
                .iter()
                .zip(&row.relay_failure_counts)
                .zip(&row.relay_failure_last)
                .map(|((source, failures), last_failed_at)| RelayLaneFailure {
                    source_id: ContentSourceId::from_uuid(*source),
                    failures: u32::try_from(*failures).unwrap_or(u32::MAX),
                    last_failed_at: *last_failed_at,
                })
                .collect();
            for failure in task_failures.remove(&row.id).unwrap_or_default() {
                if let Some(existing) = relay_failures
                    .iter_mut()
                    .find(|existing| existing.source_id == failure.source_id)
                {
                    existing.failures = existing.failures.saturating_add(failure.failures);
                    existing.last_failed_at = existing.last_failed_at.max(failure.last_failed_at);
                } else {
                    relay_failures.push(failure);
                }
            }
            Ok(CommunityRelayTarget {
                target_id: OutreachTargetId::from_uuid(row.id),
                subreddit: row.community_label,
                language: row.language,
                platform: row.platform,
                community_url: row.community_url,
                relay_failures,
                recent_threads: super::growth_intelligence::read_room_threads(&row.threads),
            })
        })
        .collect()
}

#[derive(Debug, FromRow)]
struct RelayTargetRow {
    id: Uuid,
    community_label: String,
    language: Option<String>,
    platform: String,
    community_url: Option<String>,
    relay_failure_sources: Vec<Uuid>,
    relay_failure_counts: Vec<i64>,
    relay_failure_last: Vec<OffsetDateTime>,
    threads: serde_json::Value,
}
