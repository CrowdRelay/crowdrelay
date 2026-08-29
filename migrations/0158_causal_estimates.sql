-- Causal estimates — IPW/paired estimates of treatment effects.
--
-- This table stores the output of the causal identification step: the
-- estimated treatment effect τ for each (template, subreddit_type) pair,
-- with proper variance from the IPW or paired estimator.
--
-- This is separate from attribution (`viryaos_fan_attribution`): attribution
-- answers "which action gets credit?", causal identification answers "what
-- would have happened without the action?"
--
-- This table replaces `viryaos_treatment_effect_observations` as the brain's
-- read path for treatment effects. The old table is kept for backward
-- compatibility during the transition.

CREATE TABLE IF NOT EXISTS viryaos_causal_estimates (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- The template this estimate applies to.
    template_id text NOT NULL,

    -- The subreddit type (optional — some estimates are not subreddit-specific).
    subreddit_type text,

    -- The estimated treatment effect (τ = Y(1) - Y(0)).
    tau double precision NOT NULL,

    -- The variance of the estimate (from IPW or paired estimator).
    variance double precision NOT NULL,

    -- The estimation method.
    method text NOT NULL DEFAULT 'ipw' CHECK (method IN (
        'ipw',                    -- inverse propensity weighting
        'paired',                 -- paired comparison
        'difference_in_differences'
    )),

    -- The number of observations used to compute this estimate.
    sample_size integer NOT NULL,

    -- The target horizon: 'y14_incremental' (14-day incremental fans) or
    -- 'y30_durable' (30-day durable fans).
    target text NOT NULL DEFAULT 'y14_incremental' CHECK (target IN (
        'y14_incremental',
        'y30_durable'
    )),

    computed_at timestamptz NOT NULL DEFAULT now(),

    -- One estimate per (workspace, template, subreddit_type, method, target).
    -- Use a unique index with COALESCE instead of a table-level UNIQUE constraint
    -- because PostgreSQL doesn't allow expressions in table-level UNIQUE constraints.
);

-- Indexes for the brain's causal model loading.
CREATE INDEX IF NOT EXISTS idx_causal_estimates_workspace_template
    ON viryaos_causal_estimates (workspace_id, template_id);

CREATE INDEX IF NOT EXISTS idx_causal_estimates_target
    ON viryaos_causal_estimates (workspace_id, target);

-- Unique constraint via index (supports NULL subreddit_type).
CREATE UNIQUE INDEX IF NOT EXISTS uq_causal_estimates_template_method_target
    ON viryaos_causal_estimates (workspace_id, template_id, COALESCE(subreddit_type, ''), method, target);
