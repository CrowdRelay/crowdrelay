-- Index: speed up the brain evidence view's filter for resolved predictions.
--
-- The viryaos_brain_evidence view joins viryaos_dispatch_predictions with
-- viryaos_autopilot_outcomes. The brain's read path (load_causal_model,
-- compute_and_store_treatment_effects) filters by workspace_id and
-- resolved_at IS NOT NULL. The existing index only covers unresolved
-- predictions (WHERE resolved_at IS NULL). This partial index covers the
-- resolved side, which grows as more predictions are measured.
CREATE INDEX IF NOT EXISTS idx_dispatch_predictions_resolved
    ON viryaos_dispatch_predictions (workspace_id, resolved_at)
    WHERE resolved_at IS NOT NULL;
