CREATE TABLE fan_action_tokens (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    purpose text NOT NULL CHECK (purpose IN ('confirm', 'unsubscribe')),
    token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    consumed_at timestamptz,
    UNIQUE (workspace_id, id),
    CONSTRAINT fan_action_tokens_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (expires_at > created_at),
    CHECK (consumed_at IS NULL OR consumed_at >= created_at)
);

CREATE INDEX fan_action_tokens_active_lookup_idx
    ON fan_action_tokens (workspace_id, token_hash, purpose, expires_at)
    WHERE consumed_at IS NULL;

CREATE UNIQUE INDEX fan_action_tokens_one_active_per_purpose_idx
    ON fan_action_tokens (workspace_id, fan_id, purpose)
    WHERE consumed_at IS NULL;
