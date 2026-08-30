-- Add 'unknown' to experiment_assignments execution_status.
--
-- The 'unknown' execution status represents an unresolved execution —
-- the intervention may have happened, but confirmation was lost. This
-- is distinct from 'failed' (intervention definitively did not happen)
-- and 'executed' (intervention confirmed).
--
-- Unknown invariant:
--   → excluded from realized-treatment analysis
--   → excluded from failed-treatment counts
--   → does not lower the estimated treatment effect as if failure occurred
--   → does not increase it as if execution occurred
--   → remains eligible for later resolution (non-terminal: → Executed or Failed)

ALTER TABLE viryaos_experiment_assignments
    DROP CONSTRAINT IF EXISTS viryaos_experiment_assignments_execution_status_valid;

ALTER TABLE viryaos_experiment_assignments
    ADD CONSTRAINT viryaos_experiment_assignments_execution_status_valid
    CHECK (execution_status IN (
        'control', 'withheld', 'dispatched', 'executed', 'failed', 'unknown'
    ));
