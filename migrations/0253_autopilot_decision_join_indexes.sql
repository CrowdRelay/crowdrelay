-- Two indexes for reads that already run on every operator page load.
--
-- Neither is a new capability. Both are lookups the schema has always
-- supported and never made cheap, on tables that grow with every cycle.

-- "Which action came from this decision" has no index.
--
-- `viryaos_autopilot_actions` is indexed by due time, by approval, by status,
-- by subject and by trace -- and not by the decision it came from, which is
-- the join every decision-first read makes. `/autopilot/learning-loop` runs it
-- as a LATERAL once per decision, twenty times per request;
-- `/autopilot/learning-proof` and `/ops/trace/{trace_id}` walk the same edge.
-- Each of those was a sequential scan of the actions table.
--
-- The foreign key `(workspace_id, decision_id) REFERENCES
-- viryaos_autopilot_decisions` has the same problem from the other side:
-- PostgreSQL does not index the referencing side of a foreign key, so every
-- delete or key update on a decision scanned the actions table to check it.
CREATE INDEX IF NOT EXISTS viryaos_autopilot_actions_decision_idx
    ON viryaos_autopilot_actions (workspace_id, decision_id);

-- The scorecard's measurement-pipeline section groups every measurement the
-- workspace has ever scheduled by kind and status. That is one row per action
-- per horizon, and the operator polls it. Ordering the index by
-- (workspace_id, measurement_kind, status) lets the aggregate run as an
-- index-only scan instead of reading the table.
--
-- `due_at` rides along so `min(due_at) FILTER (WHERE status = 'pending')` --
-- the "when does the next learning signal arrive" column -- is answered from
-- the same index rather than by a heap fetch per row.
CREATE INDEX IF NOT EXISTS viryaos_autopilot_measurements_kind_status_idx
    ON viryaos_autopilot_measurements (workspace_id, measurement_kind, status, due_at);
