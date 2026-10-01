//! A publication opens a fan window; finishing a draft does not close it.
//!
//! Inspect only the bounded, locked claim batch. Immature windows return to
//! pending at their actual maturity time without consuming failure attempts.
//! Other measurements in that same batch can proceed normally.

pub(super) fn claim_sql(with_tasks: bool) -> String {
    use super::observation::attributed_fans::{FIRST_TRACKED_POST, FIRST_TRACKED_POST_WITH_TASKS};

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
                   CASE WHEN tracked.first_live IS NOT NULL THEN
                       GREATEST(tracked.first_live, selected.action_finished_at)
                       + make_interval(days => CASE selected.measurement_kind
                           WHEN 'agent_run_fan_growth_3d' THEN 3
                           WHEN 'incremental_fan_growth_3d' THEN 3
                           WHEN 'durable_fan_growth_30d' THEN 44
                           ELSE 14
                         END)
                   END AS ready_at
            FROM selected
            LEFT JOIN LATERAL (
                /* first_tracked_post */
            ) AS tracked(first_live) ON true
            WHERE selected.measurement_kind IN (
                'agent_run_fan_growth_3d', 'incremental_fan_growth_3d',
                'agent_run_fan_growth_14d', 'incremental_fan_growth_14d',
                'durable_fan_growth_30d'
            )
        ), deferred AS (
            UPDATE autopilot_measurements AS measurement
            SET due_at = windows.ready_at,
                available_at = windows.ready_at,
                last_error_kind = 'awaiting_attributed_fan_window'
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
    "#
    .replace("/* first_tracked_post */", &first_live)
}
