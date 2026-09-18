-- The roster's pooled question (5.1) is "of everything my acts could do this
-- week, which five are worth doing" — and the candidates that answer it only
-- ever existed inside one workspace's eval cycle, dropped when the selection
-- was made.
--
-- `viryaos_portfolio_pool` is each act's current candidate set, written by the
-- eval at the moment the portfolio is selected. It is current state, not a
-- log: a cycle replaces its workspace's rows atomically, so the table always
-- holds the pool the latest cycle actually ranked — losers included, because a
-- candidate a lone workspace rejected for `max_dispatches` may win a slot in
-- the roster's larger pool, which is the whole point of pooling.
--
-- `decision_value` is the candidate's full economic object as JSON: a roster
-- re-rank needs the same `total()`, `resource_cost`, `uncertainty` and bridge
-- terms the act's own selection used, not a re-derived number against a world
-- model that has since moved.
--
-- Keyed (workspace_id, opportunity_key): one row per thing the brain could
-- do, replaced wholesale — so no history accumulates and no retention step is
-- needed. `refreshed_at` is the pool's staleness: an act whose eval stopped
-- shows its pool's age rather than disappearing.
CREATE TABLE viryaos_portfolio_pool (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The display form for joins and dedupe; `:` appears inside the parts, so
    -- the full identity is kept beside it as JSON rather than parsed back.
    opportunity_key text NOT NULL,
    opportunity_id jsonb NOT NULL,
    audience_key text NOT NULL,
    source_context text NOT NULL,
    action_key text NOT NULL,
    decision_value jsonb NOT NULL,
    is_experimental boolean NOT NULL DEFAULT false,
    selected boolean NOT NULL,
    rejection_reason text,
    refreshed_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, opportunity_key)
);

-- The roster read filters on workspace membership, then takes the whole pool
-- per act — the primary key's leading column is already that index.
