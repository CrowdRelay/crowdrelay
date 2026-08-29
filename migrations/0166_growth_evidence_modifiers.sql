-- Growth evidence modifiers — provenance fields for evidence quality.
-- These columns support the effective_weight computation:
--   effective_weight = base_weight(method) × contamination_factor
--                      × sample_factor × delay_factor
-- All nullable because existing rows don't have these values.
ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS sample_size integer,
    ADD COLUMN IF NOT EXISTS contamination double precision,
    ADD COLUMN IF NOT EXISTS measurement_delay_days integer;
