-- Drop the stale coarse-grained uniqueness on the credit ledger.
--
-- 0167 added `idx_credit_ledger_measurement_version` on
-- (measurement_id, attribution_version); 0171 then added the finer
-- `idx_credit_ledger_idempotent` on
-- (measurement_id, attribution_version, action_id) that the INSERT's
-- ON CONFLICT target actually matches — but never dropped the coarse one.
-- Proportional allocation writes one credit row per competing action under
-- the same measurement+version, so the second insert violates the stale
-- index. ON CONFLICT only suppresses violations on its own arbiter index,
-- so the error surfaces as unique_violation → RepositoryError::Conflict →
-- the attribution worker retried it forever (two production requests past
-- 2,000 attempts each since 2026-09-13).
--
-- `idx_credit_ledger_idempotent` already provides every guarantee the
-- coarse index did — same columns plus action_id — so the drop loses no
-- deduplication.

DROP INDEX IF EXISTS idx_credit_ledger_measurement_version;
