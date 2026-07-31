ALTER TABLE admission_pools
    ADD COLUMN slug text,
    ADD COLUMN active boolean NOT NULL DEFAULT true;

UPDATE admission_pools
SET slug = 'pool-' || substr(replace(id::text, '-', ''), 1, 12)
WHERE slug IS NULL;

ALTER TABLE admission_pools
    ALTER COLUMN slug SET NOT NULL,
    ADD CONSTRAINT admission_pools_slug_check
        CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    ADD CONSTRAINT admission_pools_workspace_event_slug_unique
        UNIQUE (workspace_id, event_id, slug);

CREATE INDEX admission_pools_active_event_idx
    ON admission_pools (workspace_id, event_id, slug)
    WHERE active;

CREATE INDEX admission_passes_public_reference_idx
    ON admission_passes (workspace_id, public_reference);

CREATE INDEX pass_sessions_active_token_idx
    ON pass_sessions (workspace_id, session_token_hash, expires_at)
    WHERE revoked_at IS NULL;
