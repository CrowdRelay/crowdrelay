-- Trace Spine: add causation_id to reach_events.
--
-- reach_events already has trace_id (migration 0187). This adds
-- causation_id so the causal chain is complete at every boundary:
--   action → reach_event (causation_id = action_id)
--
-- The column is nullable — existing rows have NULL. New writes
-- populate it from the autopilot action that caused the reach.

ALTER TABLE viryaos_reach_events
    ADD COLUMN IF NOT EXISTS causation_id uuid;

CREATE INDEX IF NOT EXISTS reach_events_causation_idx
    ON viryaos_reach_events (workspace_id, causation_id)
    WHERE causation_id IS NOT NULL;
