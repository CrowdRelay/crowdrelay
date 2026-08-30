-- Persisted experiment designs — get-or-create experiment identity.
--
-- P0-1: The experiment_uuid must survive evaluator retries. Previously the
-- evaluator generated a fresh uuid::Uuid::now_v7() each run, so a retry
-- produced a different experiment with a different deterministic roll for
-- the same logical cycle + unit. This table makes the design durable: the
-- same (workspace, intervention, logical_cycle_key) always converges on the
-- same experiment_uuid, and therefore the same assignment for each unit.
--
-- logical_cycle_key = the cooldown window bucket already encoded in
-- decision_key (now.unix_timestamp() div window_seconds). One experiment
-- design per (workspace, intervention, logical_cycle_key).
--
-- experiment_status records whether the experiment had enough power to
-- produce a meaningful randomized holdout:
--   active              — enough eligible units, randomized holdout running
--   insufficient_power  — too few units, actions run observationally only
--   completed           — measurement window closed (set by measurement worker)
CREATE TABLE IF NOT EXISTS viryaos_experiment_designs (
    experiment_uuid uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL,
    intervention_key text NOT NULL,
    logical_cycle_key text NOT NULL,
    unit_kind text NOT NULL,
    assignment_round integer NOT NULL DEFAULT 1,
    holdout_probability double precision NOT NULL,
    interference_policy text NOT NULL,
    eligible_units jsonb NOT NULL DEFAULT '[]',
    estimand jsonb NOT NULL DEFAULT '{}',
    eligibility_criteria jsonb NOT NULL DEFAULT '{}',
    selection_context jsonb NOT NULL DEFAULT '{}',
    experiment_status text NOT NULL DEFAULT 'active'
        CHECK (experiment_status IN ('active', 'insufficient_power', 'completed')),
    expected_treatment_count integer,
    expected_control_count integer,
    designed_at timestamptz NOT NULL DEFAULT now(),
    strategy text
);

-- One design per (workspace, intervention, logical_cycle_key).
-- This is the convergence guarantee: concurrent evaluators and retries
-- always resolve to the same experiment_uuid.
CREATE UNIQUE INDEX IF NOT EXISTS idx_experiment_designs_cycle
    ON viryaos_experiment_designs (workspace_id, intervention_key, logical_cycle_key);

CREATE INDEX IF NOT EXISTS idx_experiment_designs_workspace
    ON viryaos_experiment_designs (workspace_id, designed_at);

-- Link experiment assignments to their design. This enforces that every
-- assignment belongs to a persisted design — no orphan assignments.
--
-- NOT VALID: existing assignment rows from migration 0173 have random
-- experiment_uuid values (DEFAULT gen_random_uuid()) that don't reference
-- any design row. NOT VALID means the constraint only applies to new rows
-- and future updates — existing rows are exempt. A future cleanup migration
-- can backfill design rows for legacy assignments and then VALIDATE the
-- constraint.
ALTER TABLE viryaos_experiment_assignments
    DROP CONSTRAINT IF EXISTS fk_assignment_experiment;
ALTER TABLE viryaos_experiment_assignments
    ADD CONSTRAINT fk_assignment_experiment
    FOREIGN KEY (experiment_uuid) REFERENCES viryaos_experiment_designs(experiment_uuid)
    ON DELETE CASCADE
    NOT VALID;

-- Record the experiment status on each assignment so the learner can
-- distinguish active randomized holdout from insufficient-power observational
-- evidence without joining back to the design table.
ALTER TABLE viryaos_experiment_assignments
    ADD COLUMN IF NOT EXISTS experiment_status text NOT NULL DEFAULT 'active'
        CHECK (experiment_status IN ('active', 'insufficient_power', 'completed'));
