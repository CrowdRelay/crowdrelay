-- Tenant-held secrets: encrypted credentials a tenant configures through the
-- control plane (Stripe keys first) instead of redeploying env vars.
--
-- `ciphertext` is a nonce || ciphertext blob produced by
-- `crowdrelay_infra::sensitive_response::encrypt_value` under the
-- workspace-secrets key, with the workspace id and secret name authenticated
-- as associated data — a row copied to another workspace or another name
-- cannot be decrypted. `masked_hint` is computed at write time (key prefix +
-- last four) so reads can show "which key is set" without revealing it.
--
-- Plaintext never reaches this table, and the masked list is the only thing
-- the operator read path returns.

CREATE TABLE workspace_secrets (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (name ~ '^[a-z][a-z0-9_]{0,63}$'),
    ciphertext bytea NOT NULL CHECK (octet_length(ciphertext) > 24),
    masked_hint text NOT NULL CHECK (char_length(masked_hint) <= 64),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, name)
);

-- Ticketing opt-in. Tenants that exist today already run Stripe checkout in
-- production — they opted in when they went live — so they get an explicit
-- enabled row and see no behaviour change. Workspaces created after this
-- migration start with ticketing off and turn it on deliberately.
INSERT INTO tenant_settings (workspace_id, key, value)
SELECT id, 'ticketing_enabled', 'true' FROM workspaces
ON CONFLICT (workspace_id, key) DO NOTHING;
