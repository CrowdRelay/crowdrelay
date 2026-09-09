-- Per-horizon replay cursors for growth evidence.
--
-- The partial_resolution_count + last_partial_resolution_at columns (migration
-- 0247) let the brain learn from intermediate checkpoints before the full 30-day
-- outcome landed. But they introduced a double-counting bug: every measurement
-- that completes increments partial_resolution_count and stamps
-- last_partial_resolution_at, so the 3d, 14d, and 30d measurements each create a
-- new delta cursor entry. The evidence loader uses COALESCE(resolved_at,
-- last_partial_resolution_at) as the delta cursor, so the same row is replayed
-- three times. The outcome model gets updated three times from the same
-- observed_fans value — the 14d and 30d replays are no-ops for the outcome model
-- but full updates for the treatment-effect posterior, double-counting the Y14
-- observation.
--
-- This migration adds per-horizon replay timestamps so each horizon (3d, 14d,
-- 30d) is an independent observation that is learned from exactly once. The
-- delta cursor becomes the MAX of all three replay timestamps. A row is
-- eligible for delta replay if any horizon's timestamp is newer than the
-- checkpoint. The replay logic gates outcome model and treatment-effect updates
-- per horizon: the outcome model updates only when the 3d or 14d horizon is
-- new; the Y14 posterior updates only when the 14d horizon is new; the Y30
-- posterior updates only when the 30d horizon is new.
--
-- Existing rows (written before this migration) have NULL timestamps, which
-- means "never replayed under the new scheme." They will be picked up by the
-- next delta replay and learned from once per horizon that has data. The
-- partial_resolution_count and last_partial_resolution_at columns are kept for
-- backward compatibility and display — they are no longer the delta cursor.

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS replayed_3d_at timestamptz;

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS replayed_14d_at timestamptz;

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS replayed_30d_at timestamptz;

-- Index for efficiently finding evidence with new horizon data since the
-- last checkpoint. A row is "delta-eligible" when any horizon's replay
-- timestamp is newer than the checkpoint (or NULL, meaning never replayed).
CREATE INDEX IF NOT EXISTS idx_growth_evidence_replay_cursors
    ON viryaos_growth_evidence (workspace_id,
        GREATEST(COALESCE(replayed_3d_at, 'epoch'::timestamptz),
                 COALESCE(replayed_14d_at, 'epoch'::timestamptz),
                 COALESCE(replayed_30d_at, 'epoch'::timestamptz)))
    WHERE resolved_at IS NOT NULL
       OR replayed_3d_at IS NOT NULL
       OR replayed_14d_at IS NOT NULL
       OR replayed_30d_at IS NOT NULL;
