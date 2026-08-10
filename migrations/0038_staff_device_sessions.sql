-- Replace long-lived copied staff bearers with one-time pairing codes and
-- revocable per-device sessions. Static staff API keys remain a compatibility
-- fallback until telemetry confirms they are no longer used.

CREATE TABLE staff_pairing_codes (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    id uuid NOT NULL,
    code_hash bytea NOT NULL,
    display_name text NOT NULL,
    expires_at timestamptz NOT NULL,
    used_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, code_hash),
    CHECK (octet_length(code_hash) = 32),
    CHECK (length(display_name) BETWEEN 1 AND 64),
    CHECK (expires_at > created_at)
);

CREATE INDEX staff_pairing_codes_active_idx
    ON staff_pairing_codes (workspace_id, expires_at, id)
    WHERE used_at IS NULL;

CREATE TABLE staff_device_sessions (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    id uuid NOT NULL,
    token_hash bytea NOT NULL,
    display_name text NOT NULL,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, token_hash),
    CHECK (octet_length(token_hash) = 32),
    CHECK (length(display_name) BETWEEN 1 AND 64),
    CHECK (expires_at > created_at)
);

CREATE INDEX staff_device_sessions_active_token_idx
    ON staff_device_sessions (workspace_id, token_hash, expires_at)
    WHERE revoked_at IS NULL;

CREATE INDEX staff_device_sessions_admin_idx
    ON staff_device_sessions (workspace_id, created_at DESC, id DESC);
