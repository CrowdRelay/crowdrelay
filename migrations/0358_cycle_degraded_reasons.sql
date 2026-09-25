-- Why a cycle degraded, next to which phase did.
--
-- `degraded_phases` names the phase and nothing else; the reason lived in the
-- worker log. Deploys recreate the worker container and its log goes with it,
-- so on 2026-09-24 193 cycles degraded in `team_handoff_reconciliation` and
-- the cause could not be read from production at all — it turned up later in
-- an unrelated commit message (a serde-tuple timestamp cast with
-- `::timestamptz`).
--
-- One object per cycle: phase name to the database faults the worker
-- recorded up to that phase's failure. NULL for cycles before this column,
-- and for a cycle whose failing phases produced no database fault.
ALTER TABLE autopilot_cycle_runs
    ADD COLUMN IF NOT EXISTS degraded_reasons jsonb;

ALTER TABLE autopilot_cycle_runs
    ADD CONSTRAINT autopilot_cycle_runs_degraded_reasons_shape
    CHECK (degraded_reasons IS NULL OR jsonb_typeof(degraded_reasons) = 'object');
