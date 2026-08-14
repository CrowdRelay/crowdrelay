-- Durable fan push endpoints and device-acknowledged delivery ledger.
-- Rollout is fail-closed behind both runtime configuration and this DB flag.

INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
SELECT id, 'push_delivery_enabled', false, 'push provider rollout requires production smoke'
FROM workspaces
ON CONFLICT (workspace_id, key) DO NOTHING;

CREATE TABLE fan_push_endpoints (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    installation_id text NOT NULL CHECK (length(installation_id) BETWEEN 8 AND 160),
    transport text NOT NULL CHECK (transport IN ('android_fcm', 'web_push')),
    endpoint_address text NOT NULL CHECK (length(endpoint_address) BETWEEN 16 AND 4096),
    p256dh text,
    auth_secret text,
    active boolean NOT NULL DEFAULT true,
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    invalidated_at timestamptz,
    last_error_code text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT fan_push_endpoints_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT fan_push_endpoints_web_keys CHECK (
        (transport = 'android_fcm' AND p256dh IS NULL AND auth_secret IS NULL)
        OR
        (transport = 'web_push' AND p256dh IS NOT NULL AND auth_secret IS NOT NULL
            AND length(p256dh) BETWEEN 40 AND 256 AND length(auth_secret) BETWEEN 8 AND 128)
    ),
    UNIQUE (workspace_id, installation_id, transport)
);

CREATE INDEX fan_push_endpoints_active_fan_idx
    ON fan_push_endpoints (workspace_id, fan_id, transport, id)
    WHERE active AND invalidated_at IS NULL;

CREATE TABLE fan_push_deliveries (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    endpoint_id uuid NOT NULL,
    source_kind text NOT NULL CHECK (source_kind IN ('nearby_concert', 'communication_campaign')),
    source_id uuid NOT NULL,
    title text NOT NULL CHECK (length(title) BETWEEN 1 AND 160),
    body text NOT NULL CHECK (length(body) BETWEEN 1 AND 1200),
    target_path text NOT NULL CHECK (length(target_path) BETWEEN 1 AND 512),
    collapse_key text CHECK (collapse_key IS NULL OR length(collapse_key) <= 160),
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued','claimed','provider_started','provider_accepted','retry_wait','delivered','failed','ambiguous')),
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count BETWEEN 0 AND 20),
    available_at timestamptz NOT NULL DEFAULT now(),
    claim_token uuid,
    claimed_at timestamptz,
    provider_started_at timestamptz,
    provider_reference text,
    provider_accepted_at timestamptz,
    ack_token_hash bytea CHECK (ack_token_hash IS NULL OR octet_length(ack_token_hash) = 32),
    ack_deadline timestamptz,
    delivered_at timestamptz,
    completed_at timestamptz,
    error_code text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT fan_push_deliveries_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT fan_push_deliveries_endpoint_fk
        FOREIGN KEY (workspace_id, endpoint_id)
        REFERENCES fan_push_endpoints (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, source_kind, source_id, endpoint_id)
);

CREATE INDEX fan_push_deliveries_due_idx
    ON fan_push_deliveries (available_at, created_at, id)
    WHERE status IN ('queued','retry_wait');

CREATE INDEX fan_push_deliveries_ack_idx
    ON fan_push_deliveries (ack_deadline, id)
    WHERE status = 'provider_accepted';

CREATE INDEX fan_push_deliveries_fan_recent_idx
    ON fan_push_deliveries (workspace_id, fan_id, created_at DESC, id DESC);
