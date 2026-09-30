-- Compact, workspace-local continuity for completed growth evaluations.
-- No history replay and no changes to evidence or relationship authority.
ALTER TABLE brain_state DROP CONSTRAINT IF EXISTS viryaos_brain_state_module_check;
ALTER TABLE brain_state DROP CONSTRAINT IF EXISTS brain_state_module_check;
ALTER TABLE brain_state ADD CONSTRAINT brain_state_module_check CHECK (module IN (
    'treatment_effect', 'strategy_posterior', 'overlap_model', 'calibration',
    'fan_network', 'change_point', 'episode_tracker', 'causal_model',
    'reply_probability', 'metacognition'
));
