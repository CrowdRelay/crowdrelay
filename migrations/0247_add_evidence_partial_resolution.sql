-- Add partial resolution tracking to growth evidence.
--
-- The brain's learning loop was structurally broken: evidence rows only
-- became "resolved" (resolved_at IS NOT NULL) when ALL measurements (7d,
-- 14d, 30d) reached a terminal state AND the control arm resolved. This
-- meant the brain waited 30+ days before it could learn from ANY action.
--
-- This migration adds a partial_resolution_count column so the brain can
-- learn from intermediate checkpoints (7d, 14d) while waiting for the
-- full 30d outcome. The causal model loads partially resolved rows with
-- downweighted evidence quality (Observational instead of Randomized),
-- mirroring Kern's multi-checkpoint settling approach.

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS partial_resolution_count integer NOT NULL DEFAULT 0;

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS last_partial_resolution_at timestamptz;

-- Index for efficiently finding partially resolved evidence that the
-- causal model can learn from. A row is "learnable" when it has either
-- a full resolution (resolved_at IS NOT NULL) or at least one partial
-- resolution (partial_resolution_count > 0).
CREATE INDEX IF NOT EXISTS idx_growth_evidence_partial
    ON viryaos_growth_evidence (workspace_id, timestamp)
    WHERE partial_resolution_count > 0 AND resolved_at IS NULL;
