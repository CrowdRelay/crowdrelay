-- Credit ledger idempotency: link credited entries to the measurement
-- that triggered the attribution, and make writes idempotent on
-- (measurement_id, attribution_version). This allows deterministic
-- replay — rerunning attribution with a new version doesn't corrupt
-- old results.
ALTER TABLE viryaos_fan_credit_ledger
    ADD COLUMN IF NOT EXISTS measurement_id uuid,
    ADD COLUMN IF NOT EXISTS attribution_version integer NOT NULL DEFAULT 1;

-- Idempotent writes: one credit entry per (measurement_id, version, action_id).
CREATE UNIQUE INDEX IF NOT EXISTS idx_credit_ledger_idempotent
    ON viryaos_fan_credit_ledger (measurement_id, attribution_version, action_id)
    WHERE measurement_id IS NOT NULL;
