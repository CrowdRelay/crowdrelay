-- Sprint 2: Experiment runtime integrity.
--
-- P1-c: Persist the intended holdout probability alongside the realized
-- propensity. When power is insufficient and holdout is disabled, the
-- realized propensity becomes 1.0 (all treatment), but the intended
-- holdout (e.g. 0.10) is preserved for audit and causal analysis.
--
-- P1-d: The contamination_estimate column is now semantically an
-- interference_score (a coarse heuristic count, not a statistically
-- meaningful contamination probability). The DB column name stays
-- contamination_estimate for backward compatibility — the Rust struct
-- field is renamed to interference_score.

ALTER TABLE viryaos_experiment_assignments
    ADD COLUMN IF NOT EXISTS intended_holdout_probability double precision;
