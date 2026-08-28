//! Snapshot loader for the deterministic growth intelligence brain.
//!
//! Returns one snapshot per worker template that the brain may dispatch.
//! Each snapshot carries the hours since the last run and the workspace's
//! current situation (upcoming events, fan growth, unengaged targets).
//! The deterministic evaluator consumes these to decide whether to dispatch.

use super::*;
use crowdrelay_domain::growth_intelligence::{
    CommunityEngagementSummary, GrowthIntelligenceSnapshot, RecentInsight, UnengagedTarget,
};

/// The worker templates the brain may dispatch, in the order the evaluator
/// checks them. Adding a new worker template means adding it here and to the
/// evaluator's rules.
const WORKER_TEMPLATES: &[&str] = &[
    "reddit-scanner",
    "press-pitch",
    "social-post",
    "community-engager",
    "signal-inviter",
    "growth-strategist",
];

pub(in crate::autopilot) async fn load_growth_intelligence_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    _now: OffsetDateTime,
) -> Result<Vec<GrowthIntelligenceSnapshot>, RepositoryError> {
    let pool = &repo.pool;

    // Load hours since last run per template from agent_service_tasks.
    // The agent_service_tasks table is owned by the TS agent service, but
    // the brain reads it to decide when to dispatch. This is a read-only
    // cross-service query — the brain never writes to agent_service_tasks
    // directly; the executor does that via the action dispatch.
    //
    // We distinguish two timestamps:
    // - `last_any_run`: the most recent task regardless of outcome. Used for
    //   the failed-run retry delay so the brain doesn't retry every cycle.
    // - `last_effective_run`: the most recent task whose outcome produced at
    //   least one item. The agents service writes one row per item with
    //   `payload.item` (singular); an empty run writes a single row with
    //   only `payload.rationale` and no `item` key. The cooldown is measured
    //   from the last effective run, so a failed/empty run does NOT reset
    //   the cooldown.
    let last_runs: Vec<(String, Option<OffsetDateTime>, Option<OffsetDateTime>)> = sqlx::query_as(
        r#"
        SELECT ast.template_id,
               MAX(ast.created_at) AS last_any_run,
               MAX(CASE WHEN ao.payload ? 'item'
                        THEN ao.created_at END) AS last_effective_run
        FROM agent_service_tasks ast
        LEFT JOIN agent_outcomes ao ON ao.task_id = ast.id
        WHERE ast.workspace_id = $1
        GROUP BY ast.template_id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // Load workspace situation: upcoming events, fan growth, unengaged targets.
    // Only `published` events are publicly announced and promotable; the
    // `events` table has no `scheduled` status (valid: draft/published/
    // cancelled/completed), so filtering by `published` + `starts_at > now()`
    // gives us the next real upcoming show.
    let upcoming_event: Option<(Option<OffsetDateTime>,)> = sqlx::query_as(
        r#"
        SELECT MIN(starts_at) FROM events
        WHERE workspace_id = $1 AND starts_at > now() AND status = 'published'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;

    let unengaged_count: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint FROM agent_outreach_targets
        WHERE workspace_id = $1 AND status = 'promoted'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;

    // Load the actual promoted community targets with a subreddit so the
    // community-engager prompt can include concrete target_id + subreddit
    // pairs. The LLM needs these to produce social_post outcomes that
    // result in community.engage.request actions.
    let unengaged_target_rows: Vec<(uuid::Uuid, String, String)> = sqlx::query_as(
        r#"
        SELECT id, display_name, subreddit
        FROM agent_outreach_targets
        WHERE workspace_id = $1
          AND status = 'promoted'
          AND target_kind = 'community'
          AND subreddit IS NOT NULL
        ORDER BY created_at DESC
        LIMIT 20
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let unengaged_targets: Vec<UnengagedTarget> = unengaged_target_rows
        .into_iter()
        .map(|(target_id, display_name, subreddit)| UnengagedTarget {
            target_id,
            display_name,
            subreddit,
        })
        .collect();

    let now = OffsetDateTime::now_utc();
    let next_event_time = upcoming_event.and_then(|(t,)| t);
    let has_upcoming_event = next_event_time
        .map(|t| (t - now).whole_days())
        .is_some_and(|d| (0..=30).contains(&d));
    let days_to_next_event = next_event_time
        .map(|t| (t - now).whole_days())
        .filter(|d| *d >= 0)
        .map(|d| d as u32);

    // Fan growth stagnation: check if fan count has not grown in 14 days.
    // This is a simplified heuristic — the real check would compare fan
    // counts over time. For now, we use whether any new fans were added
    // in the last 14 days.
    let recent_fans: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint FROM fans
        WHERE workspace_id = $1 AND created_at > now() - interval '14 days'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    let fan_growth_stagnant = recent_fans.0 == 0;

    let unengaged = u32::try_from(unengaged_count.0.max(0)).unwrap_or(0);

    // Load unconsumed insights from agent_outcomes. The brain feeds these
    // into the next worker dispatch prompt ("here's what we already know")
    // and marks them consumed after planning. This closes the feedback loop.
    // We join with agent_service_tasks to get the template_id that produced
    // each insight, so the brain can attach insights to the right snapshot.
    let insights: Vec<(uuid::Uuid, String, String, String, String, Option<String>)> =
        sqlx::query_as(
            r#"
            SELECT ao.id,
                   COALESCE(ast.template_id, 'unknown') AS template_id,
                   ao.kind,
                   COALESCE(ao.payload->'item'->>'headline', ao.payload->'item'->>'subject', '(no headline)') AS headline,
                   COALESCE(ao.payload->'item'->>'detail', ao.payload->'item'->>'body', '') AS detail,
                   ao.payload->'item'->>'recommended_action' AS recommended_action
            FROM agent_outcomes ao
            LEFT JOIN agent_service_tasks ast ON ast.id = ao.task_id
            WHERE ao.workspace_id = $1
              AND ao.status = 'processed'
              AND ao.consumed_at IS NULL
              AND ao.kind IN ('campaign_insight', 'generic_insight', 'release_plan_note')
            ORDER BY ao.created_at DESC
            LIMIT 50
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(pool)
        .await
        .map_err(map_sqlx)?;

    let recent_insights: Vec<RecentInsight> = insights
        .into_iter()
        .map(
            |(outcome_id, template_id, kind, headline, detail, recommended_action)| RecentInsight {
                outcome_id,
                template_id,
                kind,
                headline,
                detail,
                recommended_action,
            },
        )
        .collect();

    // Load community engagement history: aggregated post performance per
    // subreddit from `community_post_metrics`. Only the latest metrics row
    // per post is used, averaged across all posts to each subreddit in the
    // last 30 days. This gives the brain a signal: "r/abc gets 45 upvotes
    // on average, r/xyz gets 0 — don't waste LLM budget there."
    let engagement_rows: Vec<(String, i64, f64, f64, f64, Option<f64>)> = sqlx::query_as(
        r#"
        WITH latest_per_post AS (
            SELECT DISTINCT ON (cpm.community_post_id)
                cpm.community_post_id,
                cpm.score,
                cpm.upvotes,
                cpm.num_comments,
                cpm.upvote_ratio,
                cp.subreddit
            FROM community_post_metrics cpm
            JOIN community_posts cp ON cp.id = cpm.community_post_id
            WHERE cp.workspace_id = $1
              AND cp.posted_at > now() - interval '30 days'
            ORDER BY cpm.community_post_id, cpm.measured_at DESC
        )
        SELECT subreddit,
               COUNT(*)::bigint AS post_count,
               AVG(score)::double precision AS avg_score,
               AVG(upvotes)::double precision AS avg_upvotes,
               AVG(num_comments)::double precision AS avg_comments,
               AVG(upvote_ratio)::double precision AS avg_upvote_ratio
        FROM latest_per_post
        GROUP BY subreddit
        ORDER BY avg_score DESC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let engagement_history: Vec<CommunityEngagementSummary> = engagement_rows
        .into_iter()
        .map(
            |(subreddit, post_count, avg_score, avg_upvotes, avg_comments, avg_upvote_ratio)| {
                CommunityEngagementSummary {
                    subreddit,
                    post_count: u32::try_from(post_count.max(0)).unwrap_or(0),
                    avg_score,
                    avg_upvotes,
                    avg_comments,
                    avg_upvote_ratio,
                }
            },
        )
        .collect();

    // Build one snapshot per worker template.
    let mut snapshots = Vec::with_capacity(WORKER_TEMPLATES.len());
    for template_id in WORKER_TEMPLATES {
        let (hours_since_last_run, hours_since_last_effective_run) = last_runs
            .iter()
            .find(|(tid, _, _)| tid == template_id)
            .map(|(_, last_any, last_effective)| {
                let any = last_any.map(|t| {
                    let delta = now - t;
                    u32::try_from(delta.whole_hours().max(0)).unwrap_or(u32::MAX)
                });
                let effective = last_effective.map(|t| {
                    let delta = now - t;
                    u32::try_from(delta.whole_hours().max(0)).unwrap_or(u32::MAX)
                });
                (any, effective)
            })
            .unwrap_or((None, None));

        // Attach insights produced by this template.
        let template_insights: Vec<RecentInsight> = recent_insights
            .iter()
            .filter(|i| i.template_id == *template_id)
            .cloned()
            .collect();

        // Attach engagement history and unengaged targets only to the
        // community-engager snapshot. Other templates don't use them, so
        // we avoid cloning the Vecs.
        let history = if *template_id == "community-engager" {
            engagement_history.clone()
        } else {
            Vec::new()
        };
        let targets = if *template_id == "community-engager" {
            unengaged_targets.clone()
        } else {
            Vec::new()
        };

        snapshots.push(GrowthIntelligenceSnapshot {
            template_id: (*template_id).to_owned(),
            hours_since_last_run,
            hours_since_last_effective_run,
            has_upcoming_event,
            days_to_next_event,
            fan_growth_stagnant,
            unengaged_outreach_targets: unengaged,
            unengaged_targets: targets,
            recent_insights: template_insights,
            community_engagement_history: history,
        });
    }

    Ok(snapshots)
}

/// Marks agent outcomes as consumed by the brain. Called after the evaluator
/// has factored the insights into its dispatch decisions. Consumed rows are
/// deleted by the retention worker after 7 days.
pub(in crate::autopilot) async fn mark_insights_consumed(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    outcome_ids: &[uuid::Uuid],
) -> Result<u64, RepositoryError> {
    if outcome_ids.is_empty() {
        return Ok(0);
    }
    let pool = &repo.pool;
    let result = sqlx::query(
        r#"
        UPDATE agent_outcomes
        SET consumed_at = now()
        WHERE workspace_id = $1
          AND id = ANY($2)
          AND consumed_at IS NULL
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(outcome_ids)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(result.rows_affected())
}
