//! Snapshot loader for the deterministic growth intelligence brain.
//!
//! Returns one snapshot per worker template that the brain may dispatch.
//! Each snapshot carries the hours since the last run and the workspace's
//! current situation (upcoming events, fan growth, unengaged targets).
//! The deterministic evaluator consumes these to decide whether to dispatch.

use super::*;
use crowdrelay_domain::growth_intelligence::GrowthIntelligenceSnapshot;

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
    let last_runs: Vec<(String, Option<OffsetDateTime>)> = sqlx::query_as(
        r#"
        SELECT template_id, MAX(created_at) AS last_run
        FROM agent_service_tasks
        WHERE workspace_id = $1
        GROUP BY template_id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // Load workspace situation: upcoming events, fan growth, unengaged targets.
    let upcoming_event: Option<(OffsetDateTime,)> = sqlx::query_as(
        r#"
        SELECT MIN(starts_at) FROM events
        WHERE workspace_id = $1 AND starts_at > now() AND status = 'scheduled'
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

    let now = OffsetDateTime::now_utc();
    let has_upcoming_event = upcoming_event
        .as_ref()
        .map(|(t,)| (*t - now).whole_days())
        .is_some_and(|d| (0..=30).contains(&d));
    let days_to_next_event = upcoming_event
        .as_ref()
        .map(|(t,)| (*t - now).whole_days())
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

    // Build one snapshot per worker template.
    let mut snapshots = Vec::with_capacity(WORKER_TEMPLATES.len());
    for template_id in WORKER_TEMPLATES {
        let hours_since_last_run = last_runs
            .iter()
            .find(|(tid, _)| tid == template_id)
            .and_then(|(_, last_run)| *last_run)
            .map(|t| {
                let delta = now - t;
                u32::try_from(delta.whole_hours().max(0)).unwrap_or(u32::MAX)
            });

        snapshots.push(GrowthIntelligenceSnapshot {
            template_id: (*template_id).to_owned(),
            hours_since_last_run,
            has_upcoming_event,
            days_to_next_event,
            fan_growth_stagnant,
            unengaged_outreach_targets: unengaged,
        });
    }

    Ok(snapshots)
}
