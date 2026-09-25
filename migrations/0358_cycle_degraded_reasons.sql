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

-- Which metric a cycle's North Star reading is in.
--
-- The self-assessment compares the earlier and later halves of 60 days of
-- readings. Nothing recorded what the readings measured, so a tenant that
-- changed its North Star had two metrics in one series: production read 2 to
-- 4 (Signal installs) until 2026-09-12, then 183 and from 2026-09-16 about
-- 680 (weighted audience). The brain assessed that as `improving` — the
-- state that raises its action budget — while no new fan arrived after
-- 2026-09-11. The series now keeps only readings in the latest reading's
-- metric; rows before this column carry none and drop out, so the brain
-- starts over as `initializing` rather than claiming a switch as growth.
ALTER TABLE autopilot_cycle_runs
    ADD COLUMN IF NOT EXISTS north_star_metric text;

ALTER TABLE autopilot_cycle_runs
    ADD CONSTRAINT autopilot_cycle_runs_north_star_metric_shape
    CHECK (north_star_metric IS NULL
           OR (btrim(north_star_metric) <> '' AND char_length(north_star_metric) <= 64));
