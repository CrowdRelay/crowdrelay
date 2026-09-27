-- What a growth-evidence row's fan columns count.
--
-- Until 2026-09-27 the fan measurements (agent_run_fan_growth_*,
-- incremental_fan_growth_*, durable_fan_growth_30d) counted every fan the
-- workspace gained in the action's window. Each dispatch was credited with
-- arrivals from any cause, and overlapping dispatches with the same ones:
-- dispatch_predictions held 145 fan-observations against 23 fans ever. From
-- this migration they count fans traced to the action's own links
-- (fan_provenance_events conversions on the action's lineage).
--
-- Every row that exists now is 'workspace_window', including rows whose
-- later horizons will be measured under the new rule: a row that mixes a
-- workspace 3-day count with an attributed 14-day one is not clean either
-- way, and the learner skips it. New rows default to 'attributed'.
--
-- Two statements on purpose: ADD COLUMN ... DEFAULT fills existing rows with
-- that default, and the second statement changes it for rows inserted after.

ALTER TABLE growth_evidence
    ADD COLUMN outcome_basis text NOT NULL DEFAULT 'workspace_window'
        CONSTRAINT growth_evidence_outcome_basis_check
        CHECK (outcome_basis IN ('workspace_window', 'attributed'));

ALTER TABLE growth_evidence
    ALTER COLUMN outcome_basis SET DEFAULT 'attributed';
