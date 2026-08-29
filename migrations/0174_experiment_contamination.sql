-- Full-window contamination tracking.
--
-- Contamination is evaluated over the ENTIRE measurement window, not just
-- at assignment time. A clean assignment can become contaminated later if
-- concurrent treatment actions occur on the same unit during the window.
--
-- `assignment_time_contamination` = snapshot at assignment time (initial estimate)
-- `final_contamination` = NULL until the measurement window closes and
--   post-assignment interference is evaluated
-- `final_evidence_quality` = the downgraded quality if contamination is high
-- `contamination_resolved_at` = when the final contamination was computed

ALTER TABLE viryaos_experiment_assignments
    ADD COLUMN IF NOT EXISTS assignment_time_contamination double precision NOT NULL DEFAULT 0.0,
    ADD COLUMN IF NOT EXISTS final_contamination double precision,
    ADD COLUMN IF NOT EXISTS final_evidence_quality text,
    ADD COLUMN IF NOT EXISTS contamination_resolved_at timestamptz;

-- Backfill assignment_time_contamination from the existing
-- contamination_estimate column so old rows have a valid initial snapshot.
UPDATE viryaos_experiment_assignments
SET assignment_time_contamination = contamination_estimate
WHERE assignment_time_contamination = 0.0
  AND contamination_estimate > 0.0;
