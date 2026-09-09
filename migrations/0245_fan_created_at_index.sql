-- Composite index on fans(workspace_id, created_at) to accelerate the
-- pre/post-window COUNT(*) queries that every measurement baseline and
-- observation runs. Without this, those queries scan the full fan table
-- for the workspace, which gets slower linearly as the fanbase grows.
--
-- The partial predicate excludes suppressed fans because they are never
-- counted in growth measurements (status != 'suppressed' is the filter the
-- observer uses), so a suppressed fan row is dead weight in the index.
CREATE INDEX IF NOT EXISTS fans_created_at_idx
    ON fans (workspace_id, created_at)
    WHERE status != 'suppressed';

-- Same index on fan_push_endpoints for the Signal install measurements:
-- the observer counts new endpoints in a 7-day window, and the baseline
-- counts new endpoints in the preceding 7 days. Both filter on
-- workspace_id + created_at + active + invalidated_at IS NULL.
CREATE INDEX IF NOT EXISTS fan_push_endpoints_created_at_idx
    ON fan_push_endpoints (workspace_id, created_at)
    WHERE active = true AND invalidated_at IS NULL;
