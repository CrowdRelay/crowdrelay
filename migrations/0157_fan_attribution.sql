-- Fan attribution — which actions contributed to which fan conversions.
--
-- Attribution (which action gets credit for a fan) and causal effect (what
-- would have happened without the action) are separate concerns. This table
-- records attribution: multi-touch attribution with decay weights.
--
-- The causal effect is stored separately in `viryaos_causal_estimates`
-- (migration 0158).

CREATE TABLE IF NOT EXISTS viryaos_fan_attribution (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- The fan who was attributed.
    fan_id uuid NOT NULL REFERENCES fans(id) ON DELETE CASCADE,

    -- The action that contributed to this fan's conversion.
    action_id uuid NOT NULL REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,

    -- Attribution weight (0.0–1.0). Sum of weights per fan = 1.0 for
    -- last-touch attribution. Multi-touch attribution may distribute
    -- weight across multiple actions.
    weight double precision NOT NULL DEFAULT 1.0,

    -- The attribution model that produced this weight.
    model text NOT NULL DEFAULT 'last_touch' CHECK (model IN (
        'last_touch',      -- 100% credit to the last action before conversion
        'first_touch',     -- 100% credit to the first action
        'linear',          -- equal weight across all actions
        'time_decay',      -- exponential decay by time before conversion
        'position_based'   -- 40/20/40 split (first/middle/last)
    )),

    attributed_at timestamptz NOT NULL DEFAULT now(),

    -- One row per (fan, action, model) — the same fan can have attributions
    -- from multiple models.
    UNIQUE (workspace_id, fan_id, action_id, model)
);

-- Indexes for the brain's attribution queries.
CREATE INDEX IF NOT EXISTS idx_fan_attribution_workspace_fan
    ON viryaos_fan_attribution (workspace_id, fan_id);

CREATE INDEX IF NOT EXISTS idx_fan_attribution_action
    ON viryaos_fan_attribution (workspace_id, action_id);

CREATE INDEX IF NOT EXISTS idx_fan_attribution_model
    ON viryaos_fan_attribution (workspace_id, model);
