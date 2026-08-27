-- Partial index for the agent scorecard's "recent results" query, which
-- orders completed actions by finished_at DESC LIMIT 10. Without this index
-- the query scans all actions for the workspace and sorts in memory.
-- The partial predicate keeps the index small: only succeeded/failed rows
-- with a finished_at are included, which is exactly what the scorecard reads.
CREATE INDEX IF NOT EXISTS viryaos_autopilot_actions_finished_idx
    ON viryaos_autopilot_actions (workspace_id, finished_at DESC, id DESC)
    WHERE status IN ('succeeded', 'failed') AND finished_at IS NOT NULL;
