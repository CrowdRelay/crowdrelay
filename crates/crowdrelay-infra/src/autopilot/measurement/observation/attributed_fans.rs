//! The fans an action earned: conversions its own live, tracked links were
//! credited with — never every fan the workspace gained in the window.
//!
//! # Why this exists
//!
//! The five fan-count kinds (`AutopilotMeasurementKind::counts_attributed_fans`)
//! used to count `fans` created in the window after the action finished. Every
//! dispatch was credited with every arrival from any cause, and overlapping
//! dispatches with the same ones: on 2026-09-27 the workspace had 23 fans
//! ever, none since 2026-09-11, and dispatches had been credited with 145.
//! The outcome model believed a dispatch brings about 2.6 fans, and the
//! worker ranking was learned from that.
//!
//! # What an action's fans are
//!
//! A conversion row names the action whose post carried the clicked link
//! (`record_community_conversion`). The measured action is often not that
//! action: an `agent.run.request` is measured, but the post belongs to the
//! `community.engage.request` its outcome created. So the count runs over the
//! action's **lineage**:
//!
//! - the action itself;
//! - actions sharing its `trace_id` (production: at most four per trace);
//! - actions created from an agent outcome whose task names this action in
//!   `metadata.action_id` — only where the agent service's table exists.
//!
//! Neither link alone is complete in production (14 and 16 of 60 community
//! posts respectively), which is why both are used.
//!
//! # What the number means
//!
//! A fan cannot click a link that was never posted, so the untreated outcome
//! is zero and the count is the effect — a lower bound, missing anyone who saw
//! the post and signed up without clicking. It is only a zero where a zero was
//! possible: a lineage with no live tracked link is abandoned as
//! `NO_TRACKED_LINK`. The window opens when the first tracked post went live,
//! not when the action finished, because a draft that waited a week for a
//! person to publish it could not convert anyone in that week.
//!
//! Known limit: a conversion credited to a child action counts for the parent
//! and for the child when both are measured. They are different templates'
//! posteriors, so this is a double count across templates, never across
//! unrelated dispatches.

use super::*;

/// The window, and whether the kind asks for fans still active thirty days
/// after arriving.
fn window_for(kind: AutopilotMeasurementKind) -> Option<(i32, bool)> {
    match kind {
        AutopilotMeasurementKind::AgentRunFanGrowth3d
        | AutopilotMeasurementKind::IncrementalFanGrowth3d => Some((3, false)),
        AutopilotMeasurementKind::AgentRunFanGrowth14d
        | AutopilotMeasurementKind::IncrementalFanGrowth14d => Some((14, false)),
        AutopilotMeasurementKind::DurableFanGrowth30d => Some((14, true)),
        _ => None,
    }
}

/// Observes one attributed-fan measurement.
pub(super) async fn observe_attributed_fans(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
    now: OffsetDateTime,
) -> Result<f64, RepositoryError> {
    let Some((window_days, durable)) = window_for(measurement.kind) else {
        return Err(RepositoryError::Unexpected);
    };
    let with_tasks = agent_tasks_table_exists(pool).await?;
    let first_live = sqlx::query_scalar::<_, Option<OffsetDateTime>>(if with_tasks {
        FIRST_TRACKED_POST_WITH_TASKS
    } else {
        FIRST_TRACKED_POST
    })
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    let Some(first_live) = first_live else {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NO_TRACKED_LINK,
        ));
    };
    let opens = first_live.max(measurement.action_finished_at);
    sqlx::query_scalar::<_, f64>(if with_tasks {
        ATTRIBUTED_FANS_WITH_TASKS
    } else {
        ATTRIBUTED_FANS
    })
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .bind(opens)
    .bind(window_days)
    .bind(durable)
    .bind(now)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)
}

/// When the lineage's first tracked post went live; NULL when none did.
/// `posted_at` is the "was live" fact in all four ledgers — a draft, a failed
/// send or a row awaiting a manual post has none.
pub(in crate::autopilot::measurement) const FIRST_TRACKED_POST: &str = r#"
    WITH lineage AS (
        SELECT $2::uuid AS action_id
        UNION
        SELECT child.id
        FROM autopilot_actions AS root
        JOIN autopilot_actions AS child
          ON child.workspace_id = root.workspace_id
         AND child.trace_id = root.trace_id
        WHERE root.workspace_id = $1 AND root.id = $2
    )
    SELECT MIN(post.posted_at)
    FROM (
        SELECT posted_at FROM community_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND posted_at IS NOT NULL AND smart_link LIKE '/l/%'
        UNION ALL
        SELECT posted_at FROM social_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND posted_at IS NOT NULL
          AND (smart_link LIKE '/l/%' OR smart_link_id IS NOT NULL)
        UNION ALL
        SELECT posted_at FROM telegram_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND posted_at IS NOT NULL
          AND (smart_link LIKE '/l/%' OR smart_link_id IS NOT NULL)
        UNION ALL
        SELECT posted_at FROM discord_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND posted_at IS NOT NULL
          AND (smart_link LIKE '/l/%' OR smart_link_id IS NOT NULL)
    ) AS post
"#;

/// [`FIRST_TRACKED_POST`] with the agent-outcome branch of the lineage.
pub(in crate::autopilot::measurement) const FIRST_TRACKED_POST_WITH_TASKS: &str = r#"
    WITH lineage AS (
        SELECT $2::uuid AS action_id
        UNION
        SELECT child.id
        FROM autopilot_actions AS root
        JOIN autopilot_actions AS child
          ON child.workspace_id = root.workspace_id
         AND child.trace_id = root.trace_id
        WHERE root.workspace_id = $1 AND root.id = $2
        UNION
        SELECT outcome.processed_action_id
        FROM agent_outcomes AS outcome
        JOIN agent_service_tasks AS task ON task.id = outcome.task_id
        WHERE outcome.workspace_id = $1
          AND outcome.processed_action_id IS NOT NULL
          AND task.workspace_id = $1
          AND task.metadata->>'action_id' = $2::text
    )
    SELECT MIN(post.posted_at)
    FROM (
        SELECT posted_at FROM community_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND posted_at IS NOT NULL AND smart_link LIKE '/l/%'
        UNION ALL
        SELECT posted_at FROM social_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND posted_at IS NOT NULL
          AND (smart_link LIKE '/l/%' OR smart_link_id IS NOT NULL)
        UNION ALL
        SELECT posted_at FROM telegram_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND posted_at IS NOT NULL
          AND (smart_link LIKE '/l/%' OR smart_link_id IS NOT NULL)
        UNION ALL
        SELECT posted_at FROM discord_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND posted_at IS NOT NULL
          AND (smart_link LIKE '/l/%' OR smart_link_id IS NOT NULL)
    ) AS post
"#;

/// Distinct fans with a conversion credited to the lineage inside the window.
/// `$5` asks for durability: still active thirty days after arriving.
const ATTRIBUTED_FANS: &str = r#"
    WITH lineage AS (
        SELECT $2::uuid AS action_id
        UNION
        SELECT child.id
        FROM autopilot_actions AS root
        JOIN autopilot_actions AS child
          ON child.workspace_id = root.workspace_id
         AND child.trace_id = root.trace_id
        WHERE root.workspace_id = $1 AND root.id = $2
    )
    SELECT COUNT(DISTINCT fan.id)::double precision
    FROM fan_provenance_events AS conversion
    JOIN fans AS fan
      ON fan.workspace_id = conversion.workspace_id
     AND fan.id = conversion.fan_id
    WHERE conversion.workspace_id = $1
      AND conversion.event_kind = 'conversion'
      AND conversion.action_id IN (SELECT action_id FROM lineage)
      AND conversion.occurred_at >= $3
      AND conversion.occurred_at < $3 + make_interval(days => $4)
      AND conversion.occurred_at <= $6
      AND fan.created_at <= $6
      AND fan.status <> 'suppressed'
      AND (NOT $5 OR (fan.status = 'active'
                      AND conversion.occurred_at + INTERVAL '30 days' <= $6))
"#;

/// [`ATTRIBUTED_FANS`] with the agent-outcome branch of the lineage.
const ATTRIBUTED_FANS_WITH_TASKS: &str = r#"
    WITH lineage AS (
        SELECT $2::uuid AS action_id
        UNION
        SELECT child.id
        FROM autopilot_actions AS root
        JOIN autopilot_actions AS child
          ON child.workspace_id = root.workspace_id
         AND child.trace_id = root.trace_id
        WHERE root.workspace_id = $1 AND root.id = $2
        UNION
        SELECT outcome.processed_action_id
        FROM agent_outcomes AS outcome
        JOIN agent_service_tasks AS task ON task.id = outcome.task_id
        WHERE outcome.workspace_id = $1
          AND outcome.processed_action_id IS NOT NULL
          AND task.workspace_id = $1
          AND task.metadata->>'action_id' = $2::text
    )
    SELECT COUNT(DISTINCT fan.id)::double precision
    FROM fan_provenance_events AS conversion
    JOIN fans AS fan
      ON fan.workspace_id = conversion.workspace_id
     AND fan.id = conversion.fan_id
    WHERE conversion.workspace_id = $1
      AND conversion.event_kind = 'conversion'
      AND conversion.action_id IN (SELECT action_id FROM lineage)
      AND conversion.occurred_at >= $3
      AND conversion.occurred_at < $3 + make_interval(days => $4)
      AND conversion.occurred_at <= $6
      AND fan.created_at <= $6
      AND fan.status <> 'suppressed'
      AND (NOT $5 OR (fan.status = 'active'
                      AND conversion.occurred_at + INTERVAL '30 days' <= $6))
"#;
