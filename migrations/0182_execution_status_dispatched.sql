-- Rename existing 'executed' values to 'dispatched'.
-- The old 'executed' meant "action row created" which is really "dispatched"
-- — durable execution intent committed, but the external intervention has
-- not yet been confirmed.
-- The new 'executed' means "external intervention actually occurred" and is
-- set by the community executor when community_posts.status = 'posted'.
UPDATE viryaos_experiment_assignments
    SET execution_status = 'dispatched'
    WHERE execution_status = 'executed';

-- Update the CHECK constraint to include 'dispatched'.
ALTER TABLE viryaos_experiment_assignments
    DROP CONSTRAINT IF EXISTS viryaos_experiment_assignments_execution_status_valid;
ALTER TABLE viryaos_experiment_assignments
    ADD CONSTRAINT viryaos_experiment_assignments_execution_status_valid
    CHECK (execution_status IN ('control', 'withheld', 'dispatched', 'executed', 'failed'));

-- Index for looking up assignments by action_id — used by the community
-- executor to transition execution_status (Dispatched → Executed / Failed).
CREATE INDEX IF NOT EXISTS viryaos_experiment_assignments_action_id_idx
    ON viryaos_experiment_assignments (workspace_id, action_id)
    WHERE action_id IS NOT NULL;
