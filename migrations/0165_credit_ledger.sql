-- Credit ledger — attributed credit for fan outcomes.
-- CRITICAL INVARIANT: GrowthEvidence.observed_incremental_fans is a RAW
-- observation and must remain IMMUTABLE. The credit ledger stores attributed
-- credit SEPARATELY. Do NOT overwrite the evidence row's observed value with
-- the credited amount.
--
-- Flow:
--   raw observation (immutable in evidence)
--       → attribution (credit ledger, separate table)
--       → credited effect (in credit ledger)
--
-- The learner consumes credited effects from the credit ledger where
-- attribution is appropriate, while raw evidence remains available for
-- replay, recalculation, and future attribution-method upgrades.
CREATE TABLE IF NOT EXISTS viryaos_fan_credit_ledger (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL,
    action_id uuid REFERENCES viryaos_autopilot_actions(id) ON DELETE SET NULL,
    experiment_id text,
    credited_incremental_y14 double precision NOT NULL,
    credited_incremental_y30 double precision,
    credit_weight double precision NOT NULL,
    attribution_confidence double precision NOT NULL,
    attribution_method text NOT NULL DEFAULT 'proportional',
    eligible_competitors text NOT NULL DEFAULT '[]',
    unattributed_residual double precision NOT NULL DEFAULT 0.0,
    evidence_quality text NOT NULL DEFAULT 'observational',
    recorded_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_credit_ledger_workspace
    ON viryaos_fan_credit_ledger (workspace_id, recorded_at);
