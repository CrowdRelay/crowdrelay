-- An append-only record of what one cycle's evidence changed in the brain's
-- beliefs.
--
-- The learning loop has been real for a while: measurements resolve growth
-- evidence, the evidence moves the strategy posterior and the hypothesis
-- lifecycle, and the next cycle reads both back. What could not be shown is
-- the causal link. `viryaos_brain_state` holds one row per module, updated in
-- place, with no history -- so "the posterior says community_first is worth
-- 4.2 fans" was answerable and "the posterior changed because of what
-- happened to action X" was not. The operator had a decision list and a
-- belief snapshot and no way to join them.
--
-- Each row here is one belief that moved, what it moved from and to, and the
-- resolved evidence rows that moved it. Evidence carries `action_id`, an
-- action carries `decision_id`, and a decision carries `trace_id` -- so from
-- a revision the whole chain back to the cycle that produced the action is a
-- join, not a reconstruction from timestamps.
--
-- Deliberately not read by any decision. Nothing here feeds back into the
-- brain; the beliefs themselves already do that. This is the operator's
-- record of the learning, which is why a write failing must never fail the
-- cycle that learned.

CREATE TABLE IF NOT EXISTS viryaos_brain_belief_revisions (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- Which belief moved. Kept narrow on purpose: a module that cannot say
    -- what its `belief_key` means does not belong in a ledger whose whole
    -- value is being joinable.
    module text NOT NULL CHECK (module IN (
        'strategy_posterior',
        'hypothesis_state'
    )),

    -- Identifies the belief within the module. For 'strategy_posterior' this
    -- is the posterior cell key, `strategy:growth_trend:event_proximity`. For
    -- 'hypothesis_state' it is the template_id.
    belief_key text NOT NULL,

    -- The belief before and after, as the module serializes it. Both are
    -- required: a revision that cannot say what the belief was is a log line,
    -- not evidence of a change.
    previous_value jsonb NOT NULL,
    current_value jsonb NOT NULL,

    -- One line an operator can read without decoding the jsonb.
    change_summary text NOT NULL,

    -- The actions whose measured outcomes moved this belief.
    --
    -- Actions rather than evidence rows: the evidence row is the measurement,
    -- and the action is what an operator can follow -- an action carries the
    -- decision, the decision carries the cycle and the trace. An array rather
    -- than a join table because a revision cites a handful of actions, the
    -- citation is written once and never edited, and nothing asks "which
    -- revisions cite action X" in a hot path.
    --
    -- Never empty in practice: a writer that cannot name what moved the belief
    -- drops the revision instead of recording an unattributable one.
    caused_by_action_ids uuid[] NOT NULL DEFAULT '{}',

    recorded_at timestamptz NOT NULL DEFAULT now()
);

-- The operator's question is "what changed recently", never "this revision by
-- id" -- the id is not known until the list has been read.
CREATE INDEX IF NOT EXISTS brain_belief_revisions_recent_idx
    ON viryaos_brain_belief_revisions (workspace_id, recorded_at DESC);

-- Following one belief through time: "how has community_first:steady:far
-- moved since it was first observed".
CREATE INDEX IF NOT EXISTS brain_belief_revisions_belief_idx
    ON viryaos_brain_belief_revisions (workspace_id, module, belief_key, recorded_at DESC);
