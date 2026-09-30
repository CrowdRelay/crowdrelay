-- A monthly baseline needs at most two points from one comparable metric.
-- The prior workspace/time index can walk unrelated historical metric runs.
-- This covering index keeps each probe proportional to its answer rather
-- than to the number of brain cycles. No historical rows are removed.
CREATE INDEX IF NOT EXISTS autopilot_cycle_runs_metric_started_idx
    ON autopilot_cycle_runs (workspace_id, north_star_metric, started_at DESC)
    INCLUDE (north_star_value)
    WHERE north_star_value IS NOT NULL;
