-- Historical data repair, not a rule. Three rows, one experiment, one time.
--
-- Migration 0228 added `experiment_assignment_id` to the evidence row and
-- backfilled it from `action_id`. That reaches the treatment arm and no
-- further: a control unit is withheld on purpose, so it has no action, so its
-- three evidence rows in the live `community-engager` holdout stayed orphaned
-- and `resolve_control_evidence` — which looks them up by assignment id —
-- could never find them. The control arm of the one randomised experiment in
-- production would have gone unmeasured, and every treated row would have
-- fallen back to a quasi-experimental label for want of a contrast.
--
-- The link here is an exact string equality, not a heuristic. Control evidence
-- writes `target_key = 'community:' || <outreach target uuid>` and the
-- assignment stores that same uuid in `unit_id`, so the two sides match
-- character for character. Nothing is inferred from timing. (The timestamps do
-- corroborate it — each evidence row was written under a second after its
-- assignment, in the same transaction — but a sub-second gap is not a key and
-- is not used as one.)
--
-- Scope is pinned deliberately:
--   * one named experiment, e0412e51-6b88-4249-99f4-d8fdaa513d66
--   * the control arm only
--   * evidence that is still unlinked, so a re-run changes nothing
--   * exactly one assignment per evidence row and one evidence row per
--     assignment, asserted below — anything ambiguous is left alone
--
-- Future control assignments do not need this. They write
-- `experiment_assignment_id` at creation, in the same transaction as the
-- assignment itself. This file exists because three rows predate that, and it
-- should never be generalised into a lineage mechanism.
DO $$
DECLARE
    target_experiment CONSTANT uuid := 'e0412e51-6b88-4249-99f4-d8fdaa513d66';
    ambiguous integer;
    repaired integer;
BEGIN
    -- Refuse to guess. If any candidate pairing is not one-to-one, this
    -- migration writes nothing at all rather than picking a side.
    SELECT count(*) INTO ambiguous
    FROM (
        SELECT evidence.id
        FROM viryaos_growth_evidence AS evidence
        JOIN viryaos_experiment_assignments AS assignment
          ON assignment.workspace_id = evidence.workspace_id
         AND assignment.experiment_uuid = target_experiment
         AND assignment.arm = 'control'
         AND evidence.target_key = 'community:' || assignment.unit_id
        WHERE evidence.treatment = 'control'
          AND evidence.experiment_assignment_id IS NULL
        GROUP BY evidence.id
        HAVING count(assignment.id) <> 1
    ) AS multi_assignment;
    IF ambiguous > 0 THEN
        RAISE EXCEPTION
            'orphaned control evidence repair aborted: % evidence row(s) match more than one control assignment',
            ambiguous;
    END IF;

    SELECT count(*) INTO ambiguous
    FROM (
        SELECT assignment.id
        FROM viryaos_experiment_assignments AS assignment
        JOIN viryaos_growth_evidence AS evidence
          ON evidence.workspace_id = assignment.workspace_id
         AND evidence.target_key = 'community:' || assignment.unit_id
         AND evidence.treatment = 'control'
         AND evidence.experiment_assignment_id IS NULL
        WHERE assignment.experiment_uuid = target_experiment
          AND assignment.arm = 'control'
        GROUP BY assignment.id
        HAVING count(evidence.id) <> 1
    ) AS multi_evidence;
    IF ambiguous > 0 THEN
        RAISE EXCEPTION
            'orphaned control evidence repair aborted: % control assignment(s) match more than one evidence row',
            ambiguous;
    END IF;

    -- Only the link is written. Every outcome column, quality label and
    -- timestamp on these rows is left exactly as it was.
    UPDATE viryaos_growth_evidence AS evidence
    SET experiment_assignment_id = assignment.id
    FROM viryaos_experiment_assignments AS assignment
    WHERE assignment.workspace_id = evidence.workspace_id
      AND assignment.experiment_uuid = target_experiment
      AND assignment.arm = 'control'
      AND evidence.target_key = 'community:' || assignment.unit_id
      AND evidence.treatment = 'control'
      AND evidence.experiment_assignment_id IS NULL;
    GET DIAGNOSTICS repaired = ROW_COUNT;

    RAISE NOTICE 'repaired % orphaned control evidence link(s) for experiment %',
        repaired, target_experiment;
END
$$;
