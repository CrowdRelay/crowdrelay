-- Synesthesia becomes an optional per-tenant module.
--
-- Backfill: every workspace that existed before this migration keeps the
-- module enabled, so rollout is a no-op for the first tenant. The gate reads
-- the flag fail-closed afterwards: a workspace WITHOUT a row (any workspace
-- created later) has no synesthesia surface until an operator enables it
-- through the existing feature-flag channel. Fan privacy actions
-- (/v1/me/synesthesia/leaderboard unpublish) stay outside the gate forever.

INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
SELECT id, 'synesthesia_module', true,
       'Backfilled for pre-existing tenants: module stays live after gating'
FROM workspaces
ON CONFLICT DO NOTHING;
