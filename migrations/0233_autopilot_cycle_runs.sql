-- One row per brain cycle, so a cycle can be asked what it did.
--
-- A cycle runs four phases in sequence -- record first-party growth metrics,
-- evaluate, execute authorized actions, claim due measurements -- and each is
-- deliberately isolated so one failing phase cannot block the others. That
-- isolation is right, and it is why nothing tied a cycle together: each phase
-- logged its own line, `phase_failed` collapsed all of them into one boolean,
-- and the only record of a cycle having happened at all was a scatter of log
-- lines with no shared identifier.
--
-- The cost showed up while diagnosing why the brain kept proposing Reddit
-- outreach nobody could act on. Every question -- which cycle produced that
-- decision, what else did that cycle do, was a phase failing, how long has this
-- been happening -- had to be reconstructed by correlating timestamps across
-- four tables and a log. `trace_id` already joins one decision to its action,
-- outbox event, delivery and evidence; nothing joined the decisions of one
-- cycle to each other.
--
-- Deliberately not a queue or a lock. Nothing reads this to decide anything.
-- It is an operator's record of what the brain did and whether it worked, which
-- is why a write failing here must never fail the cycle it describes.

CREATE TABLE IF NOT EXISTS viryaos_autopilot_cycle_runs (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- 'scheduled' when the tick fired, 'requested' when an operator asked.
    -- A cycle that only ever runs on request is a different problem from one
    -- that runs on schedule and does nothing.
    trigger text NOT NULL CHECK (trigger IN ('scheduled', 'requested')),
    started_at timestamptz NOT NULL DEFAULT now(),
    -- NULL means the cycle never finished: the process died mid-cycle, which is
    -- otherwise indistinguishable from a cycle that ran and decided nothing.
    finished_at timestamptz,
    duration_ms integer CHECK (duration_ms IS NULL OR duration_ms >= 0),
    -- 'succeeded' when every phase completed, 'degraded' when at least one
    -- phase failed and the rest carried on, which is the isolation working
    -- rather than the cycle failing.
    outcome text CHECK (outcome IS NULL OR outcome IN ('succeeded', 'degraded')),
    -- What the cycle produced, counted after the fact from the tables that
    -- carry the truth, so this can never disagree with them by drifting.
    decisions_recorded integer NOT NULL DEFAULT 0 CHECK (decisions_recorded >= 0),
    actions_created integer NOT NULL DEFAULT 0 CHECK (actions_created >= 0),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- The operator's question is always "the last few cycles", never "this cycle by
-- id" -- they do not have the id until they have the list.
CREATE INDEX IF NOT EXISTS autopilot_cycle_runs_recent_idx
    ON viryaos_autopilot_cycle_runs (workspace_id, started_at DESC);

-- Finding the cycles that went wrong has to stay cheap as the table grows,
-- because that is the only query anyone runs twice.
CREATE INDEX IF NOT EXISTS autopilot_cycle_runs_degraded_idx
    ON viryaos_autopilot_cycle_runs (workspace_id, started_at DESC)
    WHERE outcome IS DISTINCT FROM 'succeeded';
