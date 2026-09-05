-- Discard cycle North Star readings taken under the previous definition.
--
-- `close_cycle_run` used to compute `north_star_value` itself, as
-- `count(*) FROM fans WHERE status = 'active'`. It now records the reading the
-- cycle's world model resolved -- whatever metric the tenant has chosen, which
-- defaults to signal installs and is not fans.
--
-- Every row written before this migration therefore holds a number produced by
-- a different rule. Left in place they would be spliced onto the front of the
-- new series, and `crowdrelay_brain::self_assessment` compares the earlier half
-- of a window against the later half: a tenant with ten fans and one signal
-- install would show a ninety percent fall on the day of the change and be
-- reported as `regressing` for a month. That is exactly the kind of confident,
-- wrong answer the assessment exists to replace.
--
-- NULL rather than a delete: the cycle rows record what each cycle did, which
-- is still true and still worth reading. Only the reading is withdrawn, and the
-- column is nullable precisely because a cycle with no reading is a thing that
-- can happen. The series skips them, so the assessment simply restarts from
-- `initializing` and earns its next verdict from readings that mean one thing.
UPDATE viryaos_autopilot_cycle_runs
SET north_star_value = NULL
WHERE north_star_value IS NOT NULL;
