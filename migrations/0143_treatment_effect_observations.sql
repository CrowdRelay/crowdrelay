-- Treatment-effect observations for the treatment-effect posterior.
--
-- The brain's treatment-effect model P(τ|context) is updated from paired
-- treatment/control experiment outcomes. Each row stores a pre-computed τ
-- estimate (IPW estimator) and its variance, grouped by template and
-- subreddit type. This enables the brain to rank templates by their causal
-- effect (τ = Y(1) - Y(0)), not just their correlation with outcomes.
--
-- τ can be negative — the action backfired. The signed value is preserved
-- so the brain can learn to avoid harmful actions.

CREATE TABLE IF NOT EXISTS viryaos_treatment_effect_observations (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    template_id     TEXT NOT NULL,
    -- The subreddit type context (nullable for global estimates).
    subreddit_type  TEXT,
    -- The estimated treatment effect τ = E[Y|treatment] - E[Y|control].
    -- Can be negative (the action backfired).
    observed_tau    DOUBLE PRECISION NOT NULL,
    -- The variance of the τ estimate (from the IPW estimator).
    observation_variance DOUBLE PRECISION NOT NULL,
    -- Number of paired observations used to compute this estimate.
    sample_size     INTEGER NOT NULL DEFAULT 1,
    -- When this observation was computed.
    computed_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_treatment_effect_workspace
    ON viryaos_treatment_effect_observations(workspace_id, computed_at DESC);

CREATE INDEX idx_treatment_effect_template
    ON viryaos_treatment_effect_observations(workspace_id, template_id);
