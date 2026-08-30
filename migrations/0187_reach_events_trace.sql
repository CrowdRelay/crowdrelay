-- Trace Spine: add trace_id to reach_events.
--
-- This closes the trace propagation gap in the worker path. When the
-- community executor records a reach_event, it now writes the same
-- trace_id that was propagated from the autopilot action. This makes
-- the trace timeline complete:
--   decision → action → outbox → reach_event → evidence_event → measurement
--
-- The column is nullable — existing rows have NULL trace_id. New writes
-- populate it. This is safe for mixed-version deployments.

ALTER TABLE viryaos_reach_events
    ADD COLUMN IF NOT EXISTS trace_id uuid;

CREATE INDEX IF NOT EXISTS reach_events_trace_idx
    ON viryaos_reach_events (workspace_id, trace_id)
    WHERE trace_id IS NOT NULL;
