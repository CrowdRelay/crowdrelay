-- Relax the foreign key on viryaos_growth_evidence.action_id.
--
-- The randomized holdout feature records control-group evidence rows for
-- actions that were never dispatched (the worker was held out). These rows
-- have a synthetic action_id that doesn't exist in
-- viryaos_autopilot_actions — that's the point of the control group.
--
-- The FK constraint ON DELETE CASCADE is preserved for rows that DO have
-- a matching action (the treatment group). We drop and recreate the
-- constraint as ON DELETE SET NULL so that:
--   - Treatment evidence: action_id stays populated, cascades on delete.
--   - Control evidence: action_id is synthetic, no FK enforcement.
--   - If an action is deleted, its evidence rows get action_id = NULL
--     rather than being deleted (preserving the audit trail).

ALTER TABLE viryaos_growth_evidence
    DROP CONSTRAINT IF EXISTS viryaos_growth_evidence_action_id_fkey;

ALTER TABLE viryaos_growth_evidence
    ALTER COLUMN action_id DROP NOT NULL;

ALTER TABLE viryaos_growth_evidence
    ADD CONSTRAINT viryaos_growth_evidence_action_id_fkey
    FOREIGN KEY (action_id) REFERENCES viryaos_autopilot_actions(id)
    ON DELETE SET NULL;
