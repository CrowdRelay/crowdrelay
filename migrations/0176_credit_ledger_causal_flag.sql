-- Add is_causal_evidence flag to the credit ledger.
--
-- ATTRIBUTION CREDIT ≠ CAUSAL EFFECT
-- The credit ledger stores attribution artifacts, not causal evidence.
-- The is_causal_evidence flag distinguishes:
--   false = proportional attribution (default)
--   true  = backed by randomized holdout with final_contamination < 0.1
--
-- The learner must consume credit ledger entries with this distinction.
-- Only randomized/quasi-experimental evidence produces causal claims.

ALTER TABLE viryaos_fan_credit_ledger
    ADD COLUMN IF NOT EXISTS is_causal_evidence boolean NOT NULL DEFAULT false;
