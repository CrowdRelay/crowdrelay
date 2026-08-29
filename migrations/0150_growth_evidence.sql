-- Growth evidence — the unified, immutable evidence log that all learning
-- subsystems consume.
--
-- The brain has multiple learning subsystems (causal model, treatment
-- effects, reach conversion, calibration, strategy learning). Previously,
-- each had its own idea of what happened. This table provides a single
-- immutable evidence record per dispatch, capturing: prediction, context,
-- treatment assignment, propensity, reach, and outcome.
--
-- The existing viryaos_brain_evidence VIEW remains as a backward-compatible
-- projection. The brain reads from this table when available and falls
-- back to the view for historical data.
--
-- Design:
-- - One row per dispatch action. The action_id links to viryaos_autopilot_actions.
-- - Outcome fields (observed_fans, observed_incremental_fans, durable_fans_30d)
--   start NULL and are filled in when measurements arrive.
-- - The treatment and propensity fields enable IPW-based causal effect
--   estimation from the evidence log.
-- - The context jsonb stores the full DispatchContext for replay.

CREATE TABLE IF NOT EXISTS viryaos_growth_evidence (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- The autopilot action that triggered this evidence.
    action_id uuid NOT NULL REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,

    -- The stable opportunity ID (template:target:action:context_hash).
    opportunity_id text,

    -- When the evidence was recorded (dispatch time).
    timestamp timestamptz NOT NULL DEFAULT now(),

    -- Reach fields.
    audience text,
    recipient_id text NOT NULL,
    channel text NOT NULL CHECK (channel IN (
        'email', 'reddit_post', 'reddit_dm', 'signal_push',
        'social_post', 'sms', 'other'
    )),
    estimated_reach integer NOT NULL DEFAULT 1 CHECK (estimated_reach >= 1),
    actual_reach integer,

    -- Treatment assignment for causal inference.
    treatment text NOT NULL DEFAULT 'treatment' CHECK (treatment IN ('treatment', 'control')),
    propensity double precision NOT NULL DEFAULT 1.0 CHECK (propensity > 0.0 AND propensity <= 1.0),

    -- Outcome fields (filled in when measurements arrive).
    observed_fans double precision,
    observed_incremental_fans double precision,
    durable_fans_30d double precision,
    converted boolean NOT NULL DEFAULT false,
    converted_fan_id uuid REFERENCES fans(id) ON DELETE SET NULL,

    -- Prediction fields (what the brain expected before the dispatch).
    predicted_fans double precision NOT NULL DEFAULT 0.0,
    predicted_signal_installs double precision NOT NULL DEFAULT 0.0,
    context jsonb NOT NULL DEFAULT '{}'::jsonb,

    -- When the outcome was resolved (any measurement arrived).
    resolved_at timestamptz,

    created_at timestamptz NOT NULL DEFAULT now()
);

-- Indexes for the brain's most common queries.
CREATE INDEX IF NOT EXISTS idx_growth_evidence_workspace_timestamp
    ON viryaos_growth_evidence (workspace_id, timestamp DESC);

CREATE INDEX IF NOT EXISTS idx_growth_evidence_workspace_action
    ON viryaos_growth_evidence (workspace_id, action_id);

CREATE INDEX IF NOT EXISTS idx_growth_evidence_workspace_channel
    ON viryaos_growth_evidence (workspace_id, channel);

CREATE INDEX IF NOT EXISTS idx_growth_evidence_resolved
    ON viryaos_growth_evidence (workspace_id, resolved_at)
    WHERE resolved_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_growth_evidence_treatment
    ON viryaos_growth_evidence (workspace_id, treatment)
    WHERE treatment = 'control';

-- Ensure one evidence row per action.
CREATE UNIQUE INDEX IF NOT EXISTS uq_growth_evidence_action
    ON viryaos_growth_evidence (workspace_id, action_id);
