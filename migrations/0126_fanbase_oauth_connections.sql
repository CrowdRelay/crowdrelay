-- Fanbase OAuth: first-class platform connections with encrypted token
-- storage in crowdrelay's own DB.
--
-- The original fanbase_connections table (0113) stored only a credential_ref
-- pointer to n8n's credential store. For first-class OAuth (Meta, Google,
-- Spotify, Reddit, TikTok), crowdrelay now holds the encrypted tokens itself.
-- The credential_ref column remains for backward compat with n8n-backed
-- connections, but new OAuth connections store tokens in the new columns.
--
-- OAuth state table mirrors agent_service_oauth_states: PKCE verifier +
-- provider state, short TTL, single-use.

ALTER TABLE fanbase_connections
    ADD COLUMN IF NOT EXISTS encrypted_access_token text,
    ADD COLUMN IF NOT EXISTS encrypted_refresh_token text,
    ADD COLUMN IF NOT EXISTS token_expires_at timestamptz,
    ADD COLUMN IF NOT EXISTS token_scope text,
    ADD COLUMN IF NOT EXISTS token_type text DEFAULT 'bearer',
    ADD COLUMN IF NOT EXISTS account_name text,
    ADD COLUMN IF NOT EXISTS account_picture_url text;

-- OAuth state for fanbase connection flows. The PKCE verifier is stored as
-- plaintext because it is short-lived (10 minute TTL) and encrypting it would
-- require the encryption key to be available at state creation time, which
-- adds complexity for minimal security gain. The state column itself is
-- unguessable (32 random bytes, base64url-encoded).
CREATE TABLE fanbase_oauth_states (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    platform text NOT NULL CHECK (platform IN (
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown', 'spotify'
    )),
    state text NOT NULL UNIQUE,
    pkce_verifier text NOT NULL,
    redirect_uri text NOT NULL CHECK (char_length(redirect_uri) <= 512),
    expires_at timestamptz NOT NULL DEFAULT (now() + interval '10 minutes'),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX fanbase_oauth_states_workspace_idx
    ON fanbase_oauth_states (workspace_id, platform, expires_at DESC);

-- Index for cleanup queries (DELETE ... WHERE expires_at < now()).
-- A partial index with now() in the predicate is not allowed (now() is
-- STABLE, not IMMUTABLE), so this is a plain index on expires_at.
CREATE INDEX fanbase_oauth_states_expires_at_idx
    ON fanbase_oauth_states (expires_at);
