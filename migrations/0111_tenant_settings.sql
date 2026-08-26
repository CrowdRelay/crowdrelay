-- Tenant settings: per-workspace overrides for the values that used to be
-- compile-time constants of the first tenant. Absence of a row means "use the
-- shipped default", so existing deployments keep byte-identical behavior.
CREATE TABLE tenant_settings (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    key text NOT NULL CHECK (btrim(key) <> '' AND char_length(key) <= 96),
    value text NOT NULL CHECK (char_length(value) <= 512),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, key)
);
