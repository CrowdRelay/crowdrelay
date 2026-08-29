-- Add episode_id and resolved_at to viryaos_growth_evidence.
--
-- The GrowthEvidence domain type now has episode_id and resolved_at fields
-- to support the event-sourced evidence model (P1.1). The old
-- viryaos_growth_evidence table needs these columns to round-trip them.
--
-- The new viryaos_growth_episodes table (migration 0155) is the derived
-- aggregate, but the old table is kept as the source of truth during the
-- transition period.

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS episode_id text,
    ADD COLUMN IF NOT EXISTS resolved_at timestamptz;

-- Index for querying resolved evidence (the brain's learning loop).
CREATE INDEX IF NOT EXISTS idx_growth_evidence_resolved
    ON viryaos_growth_evidence (workspace_id, resolved_at)
    WHERE resolved_at IS NOT NULL;
