-- Dispatch predictions and causal model storage.
--
-- The brain predicts how many fans it expects from each worker dispatch
-- BEFORE the dispatch happens. After measurement, the prediction error
-- (observed - expected) drives learning — this is the dopamine loop.
--
-- This table stores the brain's predictions so they can be compared with
-- measured outcomes. The causal model itself (per-template expected fans)
-- is derived from these rows, not stored separately — the brain recomputes
-- it from the prediction history each cycle.

CREATE TABLE IF NOT EXISTS viryaos_dispatch_predictions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The action that was dispatched (links to viryaos_autopilot_actions).
    action_id uuid NOT NULL REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,
    -- The worker template that was dispatched (e.g. "reddit-scanner").
    template_id text NOT NULL CHECK (btrim(template_id) <> '' AND char_length(template_id) <= 64),
    -- The brain's prediction BEFORE the dispatch.
    expected_new_fans double precision NOT NULL DEFAULT 0.0 CHECK (expected_new_fans >= 0),
    expected_signal_installs double precision NOT NULL DEFAULT 0.0 CHECK (expected_signal_installs >= 0),
    -- Context features that informed the prediction (JSON for flexibility).
    context jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- The measured outcome (filled in by the measurement worker).
    observed_new_fans double precision,
    observed_signal_installs double precision,
    -- When the prediction was recorded and when it was resolved.
    predicted_at timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    -- Index for efficient lookup.
    CONSTRAINT viryaos_dispatch_predictions_unique_action UNIQUE (action_id)
);

CREATE INDEX viryaos_dispatch_predictions_workspace_template_idx
    ON viryaos_dispatch_predictions (workspace_id, template_id, predicted_at DESC);

CREATE INDEX viryaos_dispatch_predictions_unresolved_idx
    ON viryaos_dispatch_predictions (workspace_id)
    WHERE resolved_at IS NULL;
