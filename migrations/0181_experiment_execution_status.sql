-- P0: Explicit execution status — separates assignment (Z) from
-- realized execution (T). Do not derive causal execution semantics
-- from action_id NULL semantics.
--
-- arm = what was randomized (Z) — used for ITT analysis
-- execution_status = what actually happened (T) — used for per-protocol
-- action_id = linkage field only
--
-- Lifecycle:
--   control   — control arm, terminal
--   withheld  — treatment arm, portfolio did not select, terminal
--   executed  — treatment arm, action created and dispatched
--   failed    — treatment arm, action later failed (set by update_execution_status)
--
-- Monotonicity: only executed → failed is allowed.
-- Retry must never regress a finalized status.

ALTER TABLE viryaos_experiment_assignments
    ADD COLUMN IF NOT EXISTS execution_status text
        CONSTRAINT viryaos_experiment_assignments_execution_status_valid
        CHECK (execution_status IN ('control', 'withheld', 'executed', 'failed'));

-- Backfill from existing arm + action_id semantics.
-- This is the ONE time we use NULL semantics to infer execution status
-- — for legacy data only. New rows always set execution_status explicitly.
UPDATE viryaos_experiment_assignments
SET execution_status = CASE
    WHEN arm = 'control' THEN 'control'
    WHEN action_id IS NULL THEN 'withheld'
    ELSE 'executed'
END
WHERE execution_status IS NULL;

-- Now make it NOT NULL.
ALTER TABLE viryaos_experiment_assignments
    ALTER COLUMN execution_status SET NOT NULL;
