-- Add strategy and evidence_quality columns to viryaos_growth_evidence.
--
-- The GrowthEvidence domain type now records the growth strategy that was
-- active at dispatch time (so the strategy posterior learns from the real
-- strategy, not a heuristic inference) and the evidence quality (the causal
-- strength of the evidence — the learning loop weights observations by this).

ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS strategy text,
    ADD COLUMN IF NOT EXISTS evidence_quality text NOT NULL DEFAULT 'observational';

-- Backfill existing rows: they were all observational (no holdout was run).
UPDATE viryaos_growth_evidence
SET evidence_quality = 'observational'
WHERE evidence_quality IS NULL OR evidence_quality = '';
