// Included by snapshots.rs: bounded, platform-aware community selection.

pub(in crate::autopilot) async fn load_relay_community_targets(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<Vec<CommunityRelayTarget>, RepositoryError> {
    let rows = sqlx::query_as::<_, (Uuid, String, Option<String>, String, Option<String>)>(
        r#"
        SELECT t.id,
               CASE WHEN COALESCE(NULLIF(t.platform, ''), 'reddit') = 'reddit'
                    THEN t.subreddit ELSE t.display_name END,
               t.language, COALESCE(NULLIF(t.platform, ''), 'reddit'),
               COALESCE(NULLIF(t.community_url, ''), place.url)
        FROM agent_outreach_targets t
        LEFT JOIN discovery_places place
          ON place.id = t.place_id AND place.workspace_id = t.workspace_id
        LEFT JOIN discovery_place_rules rules ON rules.place_id = place.id
        LEFT JOIN LATERAL (
            SELECT MAX(cp.created_at) AS last_draft_at
            FROM community_posts cp
            WHERE cp.workspace_id = t.workspace_id AND cp.target_id = t.id
              AND cp.status IN ('pending', 'awaiting_manual_post', 'posted', 'rate_limited')
        ) last ON true
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
    Ok(rows
        .into_iter()
        .map(
            |(id, subreddit, language, platform, community_url)| CommunityRelayTarget {
                target_id: OutreachTargetId::from_uuid(id),
                subreddit,
                language,
                platform,
                community_url,
            },
        )
        .collect())
}
