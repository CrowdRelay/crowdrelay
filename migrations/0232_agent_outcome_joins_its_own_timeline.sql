-- The agent step must appear in the timeline it caused.
--
-- `/v1/admin/ops/trace/{trace_id}` reconstructs a causal chain by reading every
-- table that carries the trace, and one of its branches is:
--
--     SELECT ... FROM agent_outcomes WHERE workspace_id = $1 AND trace_id = $2
--
-- That branch has never returned a row. The agents service is the only writer
-- of `agent_outcomes` and does not populate `trace_id`, so all 67 rows in
-- production carried NULL. Migration 0231 gave the *decisions* mapped from those
-- outcomes a trace, which made the timeline coherent from the decision onward --
-- but the outcome that produced the decision was still missing from it, and that
-- outcome is the LLM step, the one an operator most wants to see.
--
-- The mapper now writes the resolved trace back onto the row it is already
-- updating when it marks the outcome processed. This backfills what came before
-- it, using the trace of the decision each outcome produced: they are the same
-- causal event, so they belong to the same trace by construction.
--
-- `processed_decision_id` is the join. An outcome that never mapped to a
-- decision -- rejected payload, or still pending -- keeps its NULL, because
-- there is no chain to place it in yet and inventing one would put a row in a
-- timeline it was never part of.
UPDATE agent_outcomes AS outcome
SET trace_id = decision.trace_id
FROM viryaos_autopilot_decisions AS decision
WHERE outcome.workspace_id = decision.workspace_id
  AND outcome.processed_decision_id = decision.id
  AND outcome.trace_id IS NULL;

-- Deliberately no NOT NULL here, unlike `viryaos_autopilot_decisions`.
--
-- That table has three writers, all in this repository, all audited. This one
-- is written by the `crowdrelay-agents` service, which lives in another
-- repository and deploys on its own schedule. Narrowing a column whose other
-- writer is outside this tree would break inserts from a service that has not
-- been changed to match, on the first outcome it produced after the migration.
--
-- The timeline branch reads `trace_id = $2`, so a NULL simply does not appear.
-- A row that fails to be enriched is invisible; a row that fails to insert is an
-- outcome lost. The nullable column is the correct trade for a table with a
-- writer this repository does not control.

-- The trace branch of the timeline filters on (workspace_id, trace_id) and had
-- no index for it, which was harmless while every value was NULL.
CREATE INDEX IF NOT EXISTS agent_outcomes_trace_idx
    ON agent_outcomes (workspace_id, trace_id)
    WHERE trace_id IS NOT NULL;
