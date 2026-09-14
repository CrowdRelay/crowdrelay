-- Why a quiet cycle was quiet, in the brain's own words.
--
-- The portfolio evaluator already computes `wait_reason` ("WAIT wins:
-- VOI=0.85 > best_action_value=0.00", "no action candidates", ...) and the
-- worker logged it. Nothing persisted it, so an operator looking at a wall of
-- `succeeded` cycles that emitted zero actions had no way to read the silence
-- — the exact gap the leverage plan's first principle forbids: the system may
-- do nothing, but it must say so.
--
-- NULL means the cycle produced actions, was skipped (park, lease race), or
-- predates this column. A recorded reason is always paired with
-- actions_created = 0; a quiet cycle with no reason is the anomaly worth
-- investigating, not the norm.
ALTER TABLE viryaos_autopilot_cycle_runs
    ADD COLUMN wait_reason text;
