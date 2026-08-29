-- Add 'causal_model' to the brain_state module CHECK constraint so the
-- brain can checkpoint its full CausalModel (outcome model, treatment
-- effects, context effects, reach model, calibration) for fast startup
-- with delta replay.
--
-- The brain loads the checkpoint on restart and applies only delta
-- evidence (evidence with timestamp > checkpoint timestamp) from
-- viryaos_growth_evidence. This is O(delta) instead of O(full history).

ALTER TABLE viryaos_brain_state
    DROP CONSTRAINT IF EXISTS viryaos_brain_state_module_check;

ALTER TABLE viryaos_brain_state
    ADD CONSTRAINT viryaos_brain_state_module_check
    CHECK (module IN (
        'treatment_effect',
        'strategy_posterior',
        'overlap_model',
        'calibration',
        'fan_network',
        'change_point',
        'episode_tracker',
        'causal_model'
    ));
