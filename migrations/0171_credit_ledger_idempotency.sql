-- Credit ledger idempotency: one credit entry per
-- (measurement_id, attribution_version, action_id). The columns themselves
-- were added in 0167_credit_ledger_attribution.sql; this migration adds a
-- finer-grained unique index that allows multiple actions to share a
-- measurement+version while still preventing duplicates.
CREATE UNIQUE INDEX IF NOT EXISTS idx_credit_ledger_idempotent
    ON viryaos_fan_credit_ledger (measurement_id, attribution_version, action_id)
    WHERE measurement_id IS NOT NULL;
