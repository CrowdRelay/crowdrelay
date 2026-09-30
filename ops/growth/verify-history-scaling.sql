-- Synthetic benchmark only: temporary history, rollback, no production reads/writes.
-- Run with psql -X -v ON_ERROR_STOP=1 -f ops/growth/verify-history-scaling.sql
BEGIN;
SET LOCAL statement_timeout = '60s';
CREATE TEMP TABLE autopilot_cycle_runs (
 workspace_id uuid, north_star_metric text, north_star_value bigint, started_at timestamptz
) ON COMMIT DROP;
CREATE INDEX history_existing_idx ON autopilot_cycle_runs(workspace_id, started_at DESC)
 WHERE north_star_value IS NOT NULL;
INSERT INTO autopilot_cycle_runs
SELECT '00000000-0000-0000-0000-000000000001'::uuid,
 CASE WHEN n % 10 = 0 THEN 'spotify_followers' ELSE 'activated_fans_30d' END,
 n, '2026-09-01 00:00:00+00'::timestamptz + n * interval '1 second'
FROM generate_series(1, 100000) AS n;
ANALYZE autopilot_cycle_runs;
-- Original monthly baseline plan:
EXPLAIN (ANALYZE, BUFFERS)
            WITH readings AS (
                SELECT started_at, north_star_value::bigint AS value
                FROM autopilot_cycle_runs
                WHERE workspace_id = '00000000-0000-0000-0000-000000000001'::uuid
                  AND north_star_metric = 'activated_fans_30d'
                  AND north_star_value IS NOT NULL
                  AND started_at <= '2026-09-30 12:00:00+00'::timestamptz
            ),
            before_month AS (
                SELECT value
                FROM readings
                WHERE started_at < date_trunc('month', '2026-09-30 12:00:00+00'::timestamptz::timestamptz)
                ORDER BY started_at DESC
                LIMIT 1
            ),
            first_in_month AS (
                SELECT value
                FROM readings
                WHERE started_at >= date_trunc('month', '2026-09-30 12:00:00+00'::timestamptz::timestamptz)
                ORDER BY started_at ASC
                LIMIT 1
            )
            SELECT COALESCE(
                (SELECT value FROM before_month),
                (SELECT value FROM first_in_month),
                20::bigint::bigint
            )::bigint;
-- A monthly baseline needs at most two points from one comparable metric.
-- The prior workspace/time index can walk unrelated historical metric runs.
-- This covering index keeps each probe proportional to its answer rather
-- than to the number of brain cycles. No historical rows are removed.
CREATE INDEX IF NOT EXISTS autopilot_cycle_runs_metric_started_idx
    ON autopilot_cycle_runs (workspace_id, north_star_metric, started_at DESC)
    INCLUDE (north_star_value)
    WHERE north_star_value IS NOT NULL;

ANALYZE autopilot_cycle_runs;
-- Bounded monthly baseline plan; same result, metric/time index expected:
EXPLAIN (ANALYZE, BUFFERS)
SELECT COALESCE(
        (
            SELECT north_star_value::bigint
            FROM autopilot_cycle_runs
            WHERE workspace_id = '00000000-0000-0000-0000-000000000001'::uuid
              AND north_star_metric = 'activated_fans_30d'
              AND north_star_value IS NOT NULL
              AND started_at < date_trunc('month', '2026-09-30 12:00:00+00'::timestamptz::timestamptz)
            ORDER BY started_at DESC
            LIMIT 1
        ),
        (
            SELECT north_star_value::bigint
            FROM autopilot_cycle_runs
            WHERE workspace_id = '00000000-0000-0000-0000-000000000001'::uuid
              AND north_star_metric = 'activated_fans_30d'
              AND north_star_value IS NOT NULL
              AND started_at >= date_trunc('month', '2026-09-30 12:00:00+00'::timestamptz::timestamptz)
              AND started_at <= '2026-09-30 12:00:00+00'::timestamptz
            ORDER BY started_at ASC
            LIMIT 1
        ),
        20::bigint::bigint
    )::bigint;
ROLLBACK;
