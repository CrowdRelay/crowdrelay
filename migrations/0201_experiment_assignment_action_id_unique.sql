-- Enforce 1:1 action-to-assignment invariant at the DB level.
--
-- The domain invariant is:
--   (workspace_id, action_id) identifies at most one experiment assignment
--   when action_id IS NOT NULL.
--
-- This is a causal-integrity invariant: the learning query must never
-- consume ambiguous causal metadata for an action. Before this migration,
-- the LATERAL ... LIMIT 1 safety net in load_growth_evidence prevented
-- fan-out but did not prevent the underlying ambiguity.
--
-- This migration:
--   1. Detects existing violations (multiple assignments for the same
--      workspace_id + action_id).
--   2. Fails loudly with diagnostics if any exist — does NOT silently
--      delete, merge, or choose a winner.
--   3. Only if zero violations, creates a partial UNIQUE INDEX.
--
-- NULL action_id remains allowed (withheld / non-dispatched assignments).

DO $$
DECLARE
    violation_count integer;
    violation_details record;
BEGIN
    -- Check for existing violations.
    SELECT COUNT(*) INTO violation_count
    FROM (
        SELECT workspace_id, action_id
        FROM viryaos_experiment_assignments
        WHERE action_id IS NOT NULL
        GROUP BY workspace_id, action_id
        HAVING COUNT(*) > 1
    ) dup;

    IF violation_count > 0 THEN
        -- Report the first few violations for diagnosis.
        FOR violation_details IN
            SELECT workspace_id::text AS ws, action_id::text AS aid, COUNT(*) AS cnt
            FROM viryaos_experiment_assignments
            WHERE action_id IS NOT NULL
            GROUP BY workspace_id, action_id
            HAVING COUNT(*) > 1
            LIMIT 10
        LOOP
            RAISE NOTICE 'DUPLICATE: workspace_id=% action_id=% count=%',
                violation_details.ws, violation_details.aid, violation_details.cnt;
        END LOOP;

        RAISE EXCEPTION
            'Cannot create unique index: % duplicate (workspace_id, action_id) pairs found. '
            'Resolve these manually before re-running this migration. '
            'Do NOT silently delete or merge assignments.',
            violation_count;
    END IF;
END $$;

-- No violations — create the partial unique index.
CREATE UNIQUE INDEX IF NOT EXISTS idx_experiment_assignments_action_id_unique
    ON viryaos_experiment_assignments (workspace_id, action_id)
    WHERE action_id IS NOT NULL;
