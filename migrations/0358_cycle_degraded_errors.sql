-- Why the phase failed, not just which one.
--
-- `degraded_phases` (migration 0261) records which of the cycle's isolated
-- phases fell over, so "which phase keeps breaking" survives the worker log
-- that named it. It does not record what the failure was: two degraded cycles
-- in the same phase can be `repository_unavailable` noise or a named conflict
-- like `org_attention_budget`, and the row could not tell them apart. On
-- 2026-09-24 a degraded cycle had to be diagnosed from the worker log its
-- phase warning was written to — which is exactly the expiry problem 0261
-- was created to solve.
--
-- One kind per phase, in the `last_error_kind`/`repository_error_kind`
-- vocabulary. A phase that iterates (actions, measurements, replies) can fail
-- on several items in one cycle; the row keeps the first kind it saw, the same
-- way `degraded_phases` already keeps one entry per phase.
--
-- An object rather than parallel arrays so `degraded_errors->>'evaluation'`
-- answers directly and the pair cannot drift out of order. NULL for every
-- row that predates this — those cycles did not record it — and an empty
-- object for a cycle where no phase failed, which stays a different
-- statement.
ALTER TABLE autopilot_cycle_runs
    ADD COLUMN IF NOT EXISTS degraded_errors jsonb;

COMMENT ON COLUMN autopilot_cycle_runs.degraded_errors IS
    'Phase identifier → error kind for each phase that failed in this cycle, in the repository_error_kind vocabulary. NULL for cycles that ran before the column existed; empty object for a cycle where no phase failed.';
