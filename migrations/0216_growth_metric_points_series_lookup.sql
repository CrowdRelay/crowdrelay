-- Growth metric points are read two ways, and neither had an index for it.
--
-- Production shows 60,451 sequential scans of `viryaos_growth_metric_points`,
-- the highest count of any table in the database. Cheap at 3,963 rows and it
-- does not stay that size: points accumulate forever, one per platform per
-- sync interval, while the query patterns stay identical.
--
-- **Window reads, by workspace and time.** The growth trends and coverage read
-- models — which the control plane hits on every page load — filter on
-- `workspace_id` and a `captured_at` window with no `series_id`, then join the
-- series afterwards:
--
--     WHERE point.workspace_id = $1
--       AND point.captured_at >= $2 - make_interval(days => $3::int)
--
-- Every existing index leads `(workspace_id, series_id, captured_at ...)`, so
-- with the second column unconstrained the planner cannot walk the time range
-- and takes a `Seq Scan` with both predicates as a filter. Confirmed by
-- EXPLAIN against production. With a single tenant this reads the whole table
-- every time.
--
-- **Per-series recency.** The sync worker's LATERAL finds each series' newest
-- point to compute the next due time. That one does use an existing index for
-- the `series_id` lookup, but the ordering is unavailable — `captured_at` sits
-- behind `workspace_id` — so the plan carries a `Sort` per series, on every
-- cycle, for every connection.
--
-- Two indexes, one for each shape. Both descending on `captured_at` because
-- every caller wants the newest first.
CREATE INDEX IF NOT EXISTS viryaos_growth_metric_points_window_by_time_idx
    ON viryaos_growth_metric_points (workspace_id, captured_at DESC);

CREATE INDEX IF NOT EXISTS viryaos_growth_metric_points_series_recent_idx
    ON viryaos_growth_metric_points (series_id, captured_at DESC);

COMMENT ON INDEX viryaos_growth_metric_points_window_by_time_idx IS
    'Serves the trends and coverage read models, which take a time window across all series in a workspace and so cannot use the series-leading indexes.';
COMMENT ON INDEX viryaos_growth_metric_points_series_recent_idx IS
    'Serves the sync worker''s per-series newest-point lookup, whose ordering the workspace-leading indexes cannot supply.';
