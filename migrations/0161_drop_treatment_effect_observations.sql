-- Drop the deprecated treatment-effect observations table.
--
-- This table was written by `compute_and_store_treatment_effects` (a dead
-- write path with zero readers) and has been superseded by the evidence-based
-- treatment-effect learning in `apply_evidence_to_model`, which uses the
-- `observed_incremental_fans` field from `viryaos_brain_evidence` as the τ
-- estimate. The table had no readers and the write path was removed.
DROP TABLE IF EXISTS viryaos_treatment_effect_observations;
