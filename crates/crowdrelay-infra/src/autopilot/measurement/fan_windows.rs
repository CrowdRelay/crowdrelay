//! A publication opens a fan window; finishing a draft does not close it.
//!
//! Inspect only the bounded, locked claim batch. Immature windows return to
//! pending at their actual maturity time without consuming failure attempts.
//! Other measurements in that same batch can proceed normally.
//!
//! # Two anchors
//!
//! The five attributed-fan kinds measure an action's *lineage*: the window
//! opens at `GREATEST(first_live, action_finished_at)` because the measured
//! action can close after its first post went live. The two content kinds
//! measure the *publication itself*: the seven days are the post's own
//! `posted_at` forward, in both directions — a post that went live before
//! its action closed does not get a window that starts late.
//!
//! All seven kinds wait for publication itself:
//! a lineage whose posts are all still publishable (pending, posting,
//! rate-limited, awaiting a manual post) is re-checked on a short clock
//! rather than claimed and abandoned as `no_tracked_link` — an abandoned
//! measurement can never see a post that lands tomorrow. Only when nothing
//! can still publish does the claim proceed, and the observer then answers
//! `no_tracked_link` honestly.

use super::observation::attributed_fans::{FIRST_TRACKED_POST, FIRST_TRACKED_POST_WITH_TASKS};

/// A lineage post that could still produce a live tracked link: pending,
/// posting, rate-limited or awaiting a manual publication. `posted`, and
/// the terminal `failed`/`cancelled`, do not qualify — one produced its
/// fact already, the others never will.
pub(in crate::autopilot::measurement) const OPEN_LINEAGE_POST: &str = r#"
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
    SELECT EXISTS (
        SELECT 1 FROM community_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND status IN ('pending', 'posting', 'rate_limited', 'awaiting_manual_post')
        UNION ALL
        SELECT 1 FROM social_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND status IN ('pending', 'posting', 'rate_limited', 'awaiting_manual_post')
        UNION ALL
        SELECT 1 FROM telegram_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND status IN ('pending', 'posting', 'rate_limited', 'awaiting_manual_post')
        UNION ALL
        SELECT 1 FROM discord_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND status IN ('pending', 'posting', 'rate_limited', 'awaiting_manual_post')
    )
"#;

/// The same open-post predicate with the production agent-outcome lineage.
pub(in crate::autopilot::measurement) const OPEN_LINEAGE_POST_WITH_TASKS: &str = r#"
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
    SELECT EXISTS (
        SELECT 1 FROM community_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND status IN ('pending', 'posting', 'rate_limited', 'awaiting_manual_post')
        UNION ALL
        SELECT 1 FROM social_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND status IN ('pending', 'posting', 'rate_limited', 'awaiting_manual_post')
        UNION ALL
        SELECT 1 FROM telegram_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND status IN ('pending', 'posting', 'rate_limited', 'awaiting_manual_post')
        UNION ALL
        SELECT 1 FROM discord_posts
        WHERE workspace_id = $1 AND action_id IN (SELECT action_id FROM lineage)
          AND status IN ('pending', 'posting', 'rate_limited', 'awaiting_manual_post')
    )
"#;

/// How often an awaiting-publication measurement resurfaces to re-check the
/// post ledger. Short enough that a post landing tomorrow gets its full
/// window; long enough that an idle queue is not churned hourly.
const REPUBLICATION_CHECK: &str = "6 hours";

/// The per-kind deferral computation inside the claim batch.
///
/// - published + window open → `ready_at = posted_at + horizon`, defer.
/// - published + window closed → `ready_at <= now`, claim and measure.
/// - nothing live but a post can still publish → `ready_at = now + 6h`,
///   defer (`awaiting_publication`).
/// - nothing live and nothing can publish → `ready_at IS NULL`, claim; the
///   observer answers `no_tracked_link`.
pub(super) fn claim_sql(with_tasks: bool) -> String {
    // Reuse the observer's exact lineage and publication definition. This is
    // static SQL substitution, not a value interpolation: tenant/id/time stay
    // bound. The second placeholder in the observer names the measured action;
    // in this correlated query it names the selected row's action instead.
    let first_live = if with_tasks {
        FIRST_TRACKED_POST_WITH_TASKS
    } else {
        FIRST_TRACKED_POST
    }
    .replace("$2", "selected.action_id");
    let open_post = if with_tasks {
        OPEN_LINEAGE_POST_WITH_TASKS
    } else {
        OPEN_LINEAGE_POST
    }
    .replace("$2", "selected.action_id");

    format!(
        r#"
        WITH selected AS MATERIALIZED (
            SELECT id, action_id, measurement_kind, action_finished_at
            FROM autopilot_measurements
            WHERE workspace_id = $1
              AND status = 'pending'
              AND due_at <= $2
              AND available_at <= $2
              AND attempt_count < 3
            ORDER BY due_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT $3
        ), windows AS MATERIALIZED (
            SELECT selected.id,
                   CASE
                       WHEN selected.measurement_kind IN (
                           'content_link_clicks_7d', 'content_fan_acquisition_7d')
                       THEN CASE
                           -- The publication's own clock: seven days from the
                           -- post's real `posted_at`, whatever the action's
                           -- finish time was.
                           WHEN tracked.first_live IS NOT NULL
                               THEN tracked.first_live + INTERVAL '7 days'
                           -- Still publishable: check again shortly rather
                           -- than abandoning a post that may land tomorrow.
                           WHEN open_post.still_open
                               THEN $2 + INTERVAL '{REPUBLICATION_CHECK}'
                           -- Nothing live, nothing left to publish — claim
                           -- so the observer answers no_tracked_link.
                           ELSE NULL
                       END
                       ELSE CASE
                         WHEN tracked.first_live IS NOT NULL THEN
                           GREATEST(tracked.first_live, selected.action_finished_at)
                           + make_interval(days => CASE selected.measurement_kind
                               WHEN 'agent_run_fan_growth_3d' THEN 3
                               WHEN 'incremental_fan_growth_3d' THEN 3
                               WHEN 'durable_fan_growth_30d' THEN 44
                               ELSE 14
                             END)
                         WHEN open_post.still_open
                             THEN $2 + INTERVAL '{REPUBLICATION_CHECK}'
                         ELSE NULL
                       END
                   END AS ready_at,
                   CASE
                       WHEN selected.measurement_kind IN (
                               'content_link_clicks_7d', 'content_fan_acquisition_7d')
                       THEN CASE
                           WHEN tracked.first_live IS NULL AND open_post.still_open
                               THEN 'awaiting_publication'
                           ELSE 'awaiting_publication_window'
                       END
                       ELSE CASE
                           WHEN tracked.first_live IS NULL AND open_post.still_open
                               THEN 'awaiting_publication'
                           ELSE 'awaiting_attributed_fan_window'
                       END
                   END AS defer_reason
            FROM selected
            LEFT JOIN LATERAL (
                /* first_tracked_post */
            ) AS tracked(first_live) ON true
            LEFT JOIN LATERAL (
                /* open_lineage_post */
            ) AS open_post(still_open) ON true
            WHERE selected.measurement_kind IN (
                'agent_run_fan_growth_3d', 'incremental_fan_growth_3d',
                'agent_run_fan_growth_14d', 'incremental_fan_growth_14d',
                'durable_fan_growth_30d',
                'content_link_clicks_7d', 'content_fan_acquisition_7d'
            )
        ), deferred AS (
            UPDATE autopilot_measurements AS measurement
            SET due_at = windows.ready_at,
                available_at = windows.ready_at,
                last_error_kind = windows.defer_reason
            FROM windows
            WHERE measurement.id = windows.id AND windows.ready_at > $2
            RETURNING measurement.id
        )
        UPDATE autopilot_measurements AS measurement
        SET status = 'processing',
            attempt_count = measurement.attempt_count + 1,
            started_at = $2,
            finished_at = NULL,
            last_error_kind = NULL
        FROM selected
        LEFT JOIN windows ON windows.id = selected.id
        WHERE measurement.id = selected.id
          AND (windows.ready_at IS NULL OR windows.ready_at <= $2)
        RETURNING measurement.id, measurement.action_id, measurement.measurement_kind,
                  measurement.subject_id, measurement.baseline_value,
                  measurement.action_finished_at, measurement.due_at,
                  measurement.attempt_count AS attempt_number
    "#,
    )
    .replace("/* first_tracked_post */", &first_live)
    .replace("/* open_lineage_post */", &open_post)
}
