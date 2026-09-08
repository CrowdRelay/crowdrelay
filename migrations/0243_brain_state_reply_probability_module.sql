-- Add 'reply_probability' to the brain_state module CHECK constraint so
-- the brain can checkpoint its ReplyProbabilityModel — the hierarchical
-- Beta-Bernoulli posterior for P(positive reply | kind, target).
--
-- The model is an additive, reversible advisory signal: it reorders
-- eligible outreach targets by predicted reply probability, it does not
-- change eligibility, authority, or approval. Deleting the checkpoint
-- (empty state) returns the system to relevance_basis_points ranking.
--
-- See crates/crowdrelay-brain/src/reply_model.rs for the full model.

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
        'causal_model',
        'reply_probability'
    ));
