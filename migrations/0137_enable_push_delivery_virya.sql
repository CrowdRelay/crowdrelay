-- Enable push delivery for the Virya workspace.
--
-- The process-level gate (CROWDRELAY_PUSH_DELIVERY_ENABLED env var) must
-- also be set to true for the worker to actually deliver pushes. This flag
-- controls the per-workspace runtime gate checked by the push delivery
-- worker on every poll cycle (push_delivery/repository.rs::feature_enabled).
--
-- Idempotent: safe to re-run. ON CONFLICT DO UPDATE ensures the flag is
-- enabled even if the row was previously inserted with enabled=false.
INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
VALUES ('6c69282c-0d60-4f18-8379-60ede34362c6', 'push_delivery_enabled', true, 'Sprint 5 activation')
ON CONFLICT (workspace_id, key) DO UPDATE
SET enabled = true, reason = EXCLUDED.reason,
    updated_at = now(), version = ecosystem_feature_flags.version + 1;
