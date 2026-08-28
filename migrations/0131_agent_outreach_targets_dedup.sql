-- Add dedup constraint to agent_outreach_targets so re-runs of the same
-- reddit-scanner task don't create duplicate staging rows. The natural
-- dedup key is (workspace_id, display_name, target_kind) — a scanner
-- suggesting the same subreddit twice should be a no-op.
--
-- Guarded by a DO block so the migration is idempotent: a partial failure
-- after the constraint is created won't block re-running the migration.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'agent_outreach_targets_workspace_name_kind_uk'
    ) THEN
        ALTER TABLE agent_outreach_targets
            ADD CONSTRAINT agent_outreach_targets_workspace_name_kind_uk
            UNIQUE (workspace_id, display_name, target_kind);
    END IF;
END $$;
