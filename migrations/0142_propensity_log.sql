-- Propensity logging for off-policy evaluation.
--
-- Every dispatch decision the brain makes is logged here with the selection
-- probability and treatment assignment. This enables inverse propensity
-- weighting (IPW) for unbiased causal effect estimation from historical data.
--
-- Without propensity logging, the brain can't distinguish "the action caused
-- fan growth" from "the brain dispatched when fan growth was about to happen
-- anyway" (selection bias).

CREATE TABLE IF NOT EXISTS viryaos_propensity_log (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id       UUID REFERENCES viryaos_autopilot_actions(id) ON DELETE SET NULL,
    opportunity_key TEXT NOT NULL,
    template_id     TEXT NOT NULL,
    -- The probability that this opportunity was selected for dispatch
    -- (from the softmax or greedy policy). Used for IPW correction.
    selection_probability DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    -- Whether this dispatch was treatment (dispatched) or control (withheld).
    treatment       TEXT NOT NULL CHECK (treatment IN ('treatment', 'control')),
    -- The probability of the treatment assignment (e.g. 0.5 for 50/50).
    assignment_probability DOUBLE PRECISION NOT NULL DEFAULT 0.5,
    -- The EFE score at the time of decision (lower = better opportunity).
    efe_score       DOUBLE PRECISION NOT NULL,
    -- The brain's policy version (for tracking changes over time).
    policy_version  INTEGER NOT NULL DEFAULT 1,
    logged_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_propensity_log_workspace
    ON viryaos_propensity_log(workspace_id, logged_at DESC);

CREATE INDEX idx_propensity_log_template
    ON viryaos_propensity_log(workspace_id, template_id, treatment);

CREATE INDEX idx_propensity_log_opportunity
    ON viryaos_propensity_log(workspace_id, opportunity_key);
