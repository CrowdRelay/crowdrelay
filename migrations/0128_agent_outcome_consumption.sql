-- Agent outcome consumption + content-hash deduplication.
--
-- The Rust autopilot (brain) now reads processed insights (campaign_insight,
-- generic_insight, release_plan_note) and feeds them into the next worker
-- dispatch prompt. After the brain factors an insight into its planning, it
-- marks the row as consumed (consumed_at). A retention job deletes consumed
-- rows after 7 days.
--
-- content_hash: a short hash of the outcome's semantic key fields, computed
-- by the TS agent service before insert. If a row with the same content_hash
-- already exists for this workspace, the insert is skipped — this prevents
-- two different tasks from producing the same idea (e.g. pitching the same
-- event to the same outlet). The hash is nullable for backward compatibility
-- with rows that predate this migration.

ALTER TABLE agent_outcomes
  ADD COLUMN IF NOT EXISTS consumed_at  TIMESTAMPTZ,
  ADD COLUMN IF NOT EXISTS content_hash TEXT CHECK (content_hash IS NULL OR char_length(content_hash) <= 64);

-- Brain loads unconsumed processed insights for the current cycle.
CREATE INDEX IF NOT EXISTS agent_outcomes_unconsumed_idx
  ON agent_outcomes (workspace_id, kind, created_at)
  WHERE consumed_at IS NULL AND status = 'processed';

-- Retention scans for rows old enough to delete after consumption.
CREATE INDEX IF NOT EXISTS agent_outcomes_consumed_retention_idx
  ON agent_outcomes (consumed_at)
  WHERE consumed_at IS NOT NULL;

-- One live idea per (workspace, content_hash). Processed/rejected rows keep
-- their hash so the brain can see "we already had this idea"; the unique
-- constraint only blocks new pending/processing duplicates. We use a partial
-- unique index so that old hash-less rows and consumed/deleted rows don't
-- block future re-evaluation of the same topic after the idea is gone.
CREATE UNIQUE INDEX IF NOT EXISTS agent_outcomes_content_hash_live_idx
  ON agent_outcomes (workspace_id, content_hash)
  WHERE content_hash IS NOT NULL AND consumed_at IS NULL;
