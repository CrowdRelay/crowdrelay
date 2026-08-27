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

-- OAuth state for fanbase connection flows. PKCE verifier is stored encrypted
-- so a DB leak cannot be used to complete a stolen authorization code.
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

-- Partial index for cleanup: only states that have expired need scanning.
CREATE INDEX fanbase_oauth_states_expired_idx
    ON fanbase_oauth_states (expires_at)
    WHERE expires_at < now();
