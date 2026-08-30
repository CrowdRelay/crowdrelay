-- Add `trace_id` to `viryaos_experiment_assignments`.
--
-- The community executor and receipt reconciliation workers backfill
-- `trace_id` from the linked autopilot action when transitioning an
-- assignment's execution_status (dispatched → executed/failed/unknown).
-- The column was referenced by the workers but never created, which would
-- raise `column "trace_id" does not exist` at runtime.
--
-- `trace_id` is nullable: legacy assignments created before this migration
-- have no trace context, and the backfill uses COALESCE so a non-null value
-- is never overwritten.

ALTER TABLE viryaos_experiment_assignments
    ADD COLUMN IF NOT EXISTS trace_id uuid;

CREATE INDEX IF NOT EXISTS idx_experiment_assignments_trace
    ON viryaos_experiment_assignments (trace_id)
    WHERE trace_id IS NOT NULL;
