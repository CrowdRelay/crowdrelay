-- Which phase failed, not just that one did.
--
-- `viryaos_autopilot_cycle_runs.outcome` is `succeeded` or `degraded`, and
-- `degraded` came from a single boolean that eighteen call sites in the
-- autopilot cycle could set. The row recorded that a phase fell over and never
-- which one; the cycle logged one line saying "a phase failed", and the phase's
-- own warning was a separate line somewhere above it.
--
-- Production on 2026-09-13: 296 cycles in 24 hours, 40 of them degraded. The
-- only way to learn what those 40 hit was to grep worker logs by timestamp,
-- which means the answer is gone as soon as the logs roll. `/v1/admin/ops/cycles
-- ?state=degraded` could list them and could not explain a single one.
--
-- `degraded` rather than `failed` is deliberate — the phases are isolated, so
-- one failing while the rest complete is the design working. That is exactly why
-- the phase name matters: without it, a 13% degraded rate is either routine
-- isolation doing its job or a broken phase failing every cycle, and the two
-- call for opposite responses.
--
-- Null for every row that predates this, which is honest: those cycles did not
-- record it. An empty array is a different statement — a cycle that completed
-- with no phase failing — so the two must not collapse.
ALTER TABLE viryaos_autopilot_cycle_runs
    ADD COLUMN IF NOT EXISTS degraded_phases text[];

COMMENT ON COLUMN viryaos_autopilot_cycle_runs.degraded_phases IS
    'Stable phase identifiers that failed in this cycle. NULL for cycles that ran before the column existed; empty for a cycle where no phase failed.';

-- The operator question is "which phase keeps breaking", which reads the array
-- of recent degraded cycles. Partial, because degraded cycles are the small set
-- and the only ones this answers for.
CREATE INDEX IF NOT EXISTS viryaos_autopilot_cycle_runs_degraded_phases_idx
    ON viryaos_autopilot_cycle_runs (workspace_id, started_at DESC)
    WHERE outcome = 'degraded';
