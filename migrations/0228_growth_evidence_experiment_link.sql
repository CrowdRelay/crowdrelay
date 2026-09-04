-- Name the experiment assignment on the evidence row.
--
-- The link between evidence and the randomisation that produced it was
-- inferred from `action_id`. That works for the treatment arm and fails
-- completely for control: a control unit is deliberately never dispatched, so
-- it has no action, so its evidence row carries `action_id IS NULL` and could
-- not be joined back to its own assignment. Production holds three such rows.
--
-- The consequence was not a missing join but a false label. Those rows say
-- `evidence_quality = 'randomized_holdout'`, and nothing could reach the
-- assignment to find out whether contamination had ever been evaluated — so
-- treatment-only data was being read as an intent-to-treat comparison against
-- a control arm that contributed no outcome at all.
ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS experiment_assignment_id text;

-- Backfill the treatment arm, where the action already establishes the link.
-- Control rows stay NULL: there is no fact in the row that identifies which
-- assignment wrote it, and inventing one from timestamps would be a guess
-- wearing the shape of a key. New control evidence carries the id directly.
UPDATE viryaos_growth_evidence AS evidence
SET experiment_assignment_id = assignment.id
FROM viryaos_experiment_assignments AS assignment
WHERE evidence.experiment_assignment_id IS NULL
  AND evidence.action_id IS NOT NULL
  AND assignment.workspace_id = evidence.workspace_id
  AND assignment.action_id = evidence.action_id;

CREATE INDEX IF NOT EXISTS viryaos_growth_evidence_assignment_idx
    ON viryaos_growth_evidence (workspace_id, experiment_assignment_id)
    WHERE experiment_assignment_id IS NOT NULL;

-- The control-arm outcome sweep looks for control assignments of an experiment
-- whose measurement window has elapsed.
CREATE INDEX IF NOT EXISTS viryaos_experiment_assignments_arm_idx
    ON viryaos_experiment_assignments (workspace_id, experiment_uuid, arm, assigned_at);
