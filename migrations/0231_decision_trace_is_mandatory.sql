-- A decision must say what caused it.
--
-- `viryaos_autopilot_decisions` is the audit ledger: the row an operator reads
-- to answer "why did the system do this?". `trace_id` is what joins it to the
-- event that produced it, the action it emitted, the attempts that action made
-- and the outcome that came back. It was nullable, and the half of the system
-- that most needs auditing was writing NULL into it.
--
-- Measured in production before this migration: of 120 decisions in the last
-- seven days, 57 had no trace. Every one of those came from the same source --
-- all 42 decisions with `subject_kind = 'agent_outcome'` were untraced, because
-- all 67 rows in `agent_outcomes` carry a NULL `trace_id` and the mapper bound
-- it straight through. So the non-deterministic limb of the system, the LLM
-- agents, produced decisions that could not be correlated to anything, while
-- the deterministic paths (`team_assignment`, `workspace`, `outreach`,
-- `growth_metrics`, `show_operations`) were traced correctly.
--
-- The correlation was never lost, only unrecorded: an outcome names its task,
-- and the task's metadata names the dispatching action, which has the trace.
-- The mapper now resolves it and the dispatcher now stamps it into the task, so
-- from here every writer supplies a trace. This makes that permanent, because a
-- nullable column is an invitation for the next writer to skip it.
--
-- No DEFAULT is added on purpose. A default would let a writer omit the trace
-- and receive a meaningless random root, which hides exactly the defect this
-- migration exists to prevent. Omitting it must fail loudly instead.

-- Backfill decisions first: actions carry a foreign key to them, and the action
-- backfill below reads the value set here.
--
-- An action's trace is the truthful answer where one exists. Where it does not,
-- the decision becomes its own trace root -- which states "this decision has no
-- recorded upstream cause", true of every row written before the dispatcher
-- carried the trace, rather than inventing a correlation that never happened.
UPDATE viryaos_autopilot_decisions AS decision
SET trace_id = COALESCE(
        (
            SELECT action.trace_id
            FROM viryaos_autopilot_actions AS action
            WHERE action.workspace_id = decision.workspace_id
              AND action.decision_id = decision.id
              AND action.trace_id IS NOT NULL
            ORDER BY action.created_at
            LIMIT 1
        ),
        decision.id
    )
WHERE trace_id IS NULL;

-- Actions inherit their decision's trace. Left nullable: this table has writers
-- whose call sites have not been audited the way the ledger's three have, and
-- narrowing a column whose writers are not all known is how a migration takes
-- production down on the next insert.
UPDATE viryaos_autopilot_actions AS action
SET trace_id = decision.trace_id
FROM viryaos_autopilot_decisions AS decision
WHERE action.workspace_id = decision.workspace_id
  AND action.decision_id = decision.id
  AND action.trace_id IS NULL;

-- Safe now: the backfill above leaves no NULL, and all three writers
-- (`autopilot/decisions/persist.rs`, `autopilot/team.rs`,
-- `worker/agent_outcomes.rs`) bind a non-optional value.
-- `scripts/test_decision_trace_contract_v1.py` keeps that true.
ALTER TABLE viryaos_autopilot_decisions
    ALTER COLUMN trace_id SET NOT NULL;

-- The partial index existed because the column was nullable. It cannot be now,
-- so the predicate only costs the planner a decision it no longer has to make.
DROP INDEX IF EXISTS autopilot_decisions_trace_idx;
CREATE INDEX IF NOT EXISTS autopilot_decisions_trace_idx
    ON viryaos_autopilot_decisions (workspace_id, trace_id);
