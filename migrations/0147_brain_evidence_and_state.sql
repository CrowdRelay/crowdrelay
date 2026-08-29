-- Brain evidence view + materialized state table.
--
-- Phase 0.1: Close the prediction→measurement loop.
--
-- The brain records predictions in viryaos_dispatch_predictions and reads
-- them back in load_causal_model. But the measurement system writes
-- observed outcomes to viryaos_autopilot_outcomes — a separate table that
-- never feeds back into dispatch_predictions. This means the brain's
-- observed_new_fans / resolved_at columns are never populated, and the
-- causal model learns from an empty dataset every cycle.
--
-- This migration:
-- 1. Creates a VIEW that joins predictions to outcomes by action_id, so
--    the brain can read unified evidence without a separate bridge step.
-- 2. Creates a materialized brain-state table for expensive-to-recompute
--    posterior state (treatment effects, strategy, overlap, calibration).
-- 3. Adds an index on viryaos_autopilot_outcomes(action_id, metric_key)
--    to make the view fast.

-- Index: speed up the join from outcomes to predictions by action_id.
CREATE INDEX IF NOT EXISTS idx_autopilot_outcomes_action_metric
    ON viryaos_autopilot_outcomes (action_id, metric_key, observed_at DESC)
    WHERE action_id IS NOT NULL;

-- View: unified prediction + outcome evidence for the brain.
--
-- The brain reads from this view instead of raw viryaos_dispatch_predictions.
-- Each row is a prediction with its observed outcome (if resolved).
-- The LEFT JOIN LATERAL picks the most recent outcome per metric per action.
--
-- observed_new_fans comes from effect.agent_run_fan_growth_14d.
-- observed_signal_installs comes from effect.agent_run_signal_installs_7d.
-- incremental_fan_growth comes from effect.incremental_fan_growth_14d.
CREATE OR REPLACE VIEW viryaos_brain_evidence AS
SELECT
    p.workspace_id,
    p.action_id,
    p.template_id,
    p.context,
    p.expected_new_fans,
    p.expected_signal_installs,
    p.predicted_at,
    fan_growth.observed_value AS observed_new_fans,
    signal_installs.observed_value AS observed_signal_installs,
    incremental.observed_value AS observed_incremental_fans,
    COALESCE(fan_growth.observed_at, signal_installs.observed_at, incremental.observed_at) AS resolved_at
FROM viryaos_dispatch_predictions p
LEFT JOIN LATERAL (
    SELECT obs.observed_value, obs.observed_at
    FROM viryaos_autopilot_outcomes obs
    WHERE obs.action_id = p.action_id
      AND obs.metric_key = 'effect.agent_run_fan_growth_14d'
    ORDER BY obs.observed_at DESC
    LIMIT 1
) fan_growth ON true
LEFT JOIN LATERAL (
    SELECT obs.observed_value, obs.observed_at
    FROM viryaos_autopilot_outcomes obs
    WHERE obs.action_id = p.action_id
      AND obs.metric_key = 'effect.agent_run_signal_installs_7d'
    ORDER BY obs.observed_at DESC
    LIMIT 1
) signal_installs ON true
LEFT JOIN LATERAL (
    SELECT obs.observed_value, obs.observed_at
    FROM viryaos_autopilot_outcomes obs
    WHERE obs.action_id = p.action_id
      AND obs.metric_key = 'effect.incremental_fan_growth_14d'
    ORDER BY obs.observed_at DESC
    LIMIT 1
) incremental ON true;

-- Table: materialized brain state for fast startup.
--
-- The brain recomputes most state from evidence each cycle. But some
-- posteriors are expensive to recompute (treatment effects, strategy,
-- overlap). This table stores serialized posterior state that can be
-- loaded on startup for fast decisions, with periodic full replay for
-- audit and correction.
--
-- The state column is a jsonb blob containing the serialized posterior.
-- The updated_at column tracks when the state was last materialized.
CREATE TABLE IF NOT EXISTS viryaos_brain_state (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    module text NOT NULL CHECK (module IN (
        'treatment_effect',
        'strategy_posterior',
        'overlap_model',
        'calibration',
        'fan_network',
        'change_point',
        'episode_tracker'
    )),
    state jsonb NOT NULL DEFAULT '{}'::jsonb,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, module)
);

-- Add unique constraint on treatment-effect observations so we can upsert.
-- This allows compute_and_store_treatment_effects to update existing
-- observations when new data arrives.
CREATE UNIQUE INDEX IF NOT EXISTS uq_treatment_effect_template_subreddit
    ON viryaos_treatment_effect_observations (workspace_id, template_id, COALESCE(subreddit_type, ''));

