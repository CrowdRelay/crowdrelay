-- What the North Star read when each brain cycle ran.
--
-- `viryaos_autopilot_cycle_runs` records what a cycle did. It cannot say
-- whether any of it worked, because "every phase completed" and "the brain
-- achieved anything" were the same field: sixteen consecutive cycles reported
-- `succeeded` while the fan count sat at ten, and an operator had to infer the
-- difference. The obvious inference is that the thing is broken, and it was
-- wrong -- the brain was starved, not faulty.
--
-- One column fixes that. With the North Star stored per cycle,
-- `crowdrelay_brain::self_assessment` can compare the last stretch of days
-- against the one before and say which of improving, learning, stagnant or
-- regressing the brain is actually in.
--
-- Nullable: a cycle whose reading could not be taken records nothing rather
-- than a zero, because a zero here is indistinguishable from having no fans and
-- would drag the trend down on a measurement failure.
ALTER TABLE viryaos_autopilot_cycle_runs
    ADD COLUMN IF NOT EXISTS north_star_value integer
        CHECK (north_star_value IS NULL OR north_star_value >= 0);

-- The assessment reads the last few weeks, newest first, and nothing else ever
-- filters on this column.
CREATE INDEX IF NOT EXISTS autopilot_cycle_runs_north_star_idx
    ON viryaos_autopilot_cycle_runs (workspace_id, started_at DESC)
    WHERE north_star_value IS NOT NULL;
