-- Experiment identity — decouple experiment identity from action identity.
--
-- EXPERIMENT ID ≠ ACTION ID
--
-- The old schema used `id` (text PK) = experiment_id = "exp:{template}:{unit}",
-- which meant:
--   1. Treatment and control arms of the same experiment shared the same
--      PK, so ON CONFLICT (id) DO NOTHING silently dropped the second arm.
--   2. The same unit dispatched in cycle 2 was silently dropped because
--      the experiment_id was the same.
--
-- The new schema:
--   - `id` (text PK) = unique assignment ID per row ("asgn:{uuid}")
--   - `experiment_uuid` = links all assignments in the same experiment
--   - `assignment_round` = increments across cycles for the same experiment
--   - One assignment per (experiment_uuid, assignment_round, unit_id) — arm
--     is a property of the assignment, not a separate row.
--   - `eligibility_criteria` and `selection_context` record the estimand
--     (effect among eligible/selected candidates, not all opportunities).
--   - `interference_policy` determines isolatability from intervention type,
--     not just unit kind declaration.

ALTER TABLE viryaos_experiment_assignments
    ADD COLUMN IF NOT EXISTS experiment_uuid uuid DEFAULT gen_random_uuid(),
    ADD COLUMN IF NOT EXISTS assignment_round integer NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS eligibility_criteria jsonb NOT NULL DEFAULT '{}',
    ADD COLUMN IF NOT EXISTS selection_context jsonb NOT NULL DEFAULT '{}',
    ADD COLUMN IF NOT EXISTS interference_policy text NOT NULL DEFAULT 'unknown';

-- One assignment per (experiment_uuid, assignment_round, unit_id).
-- This prevents accidental double-assignment of the same unit in the
-- same round. The arm is a property of the assignment.
CREATE UNIQUE INDEX IF NOT EXISTS idx_experiment_assignments_unique_unit
    ON viryaos_experiment_assignments (workspace_id, experiment_uuid, assignment_round, unit_id);

-- Index for looking up all assignments in an experiment.
CREATE INDEX IF NOT EXISTS idx_experiment_assignments_experiment
    ON viryaos_experiment_assignments (workspace_id, experiment_uuid, assigned_at);
