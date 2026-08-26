-- Fanbases: first-class audience blocks.
--
-- A fanbase is an addressable audience with its own acquisition origin, its
-- own membership ledger and its own consent posture. Campaigns target a
-- fanbase; the provider that fills it is swappable data, not architecture —
-- so when the business shifts platforms or leaves music entirely, only the
-- source rows change.
--
-- Table order matters here: connections first, then fanbases (which may point
-- at a connection), then ingestion ledger and membership.

CREATE TABLE fanbase_connections (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    platform text NOT NULL CHECK (platform IN (
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown'
    )),
    external_account_ref text NOT NULL CHECK (
        btrim(external_account_ref) <> '' AND char_length(external_account_ref) <= 200),
    -- Points at the workspace secret store entry holding the token; the
    -- credential itself never lives in this database.
    credential_ref text NOT NULL CHECK (
        btrim(credential_ref) <> '' AND char_length(credential_ref) <= 200),
    status text NOT NULL DEFAULT 'connected'
        CHECK (status IN ('connected', 'expired', 'disconnected')),
    label text NOT NULL CHECK (btrim(label) <> '' AND char_length(label) <= 200),
    last_sync_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, platform, external_account_ref)
);

CREATE INDEX fanbase_connections_workspace_idx
    ON fanbase_connections (workspace_id, platform, status);

CREATE TABLE fanbases (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 200),
    source_kind text NOT NULL CHECK (source_kind IN (
        'http_json_pull', 'csv_inline', 'manual_import', 'meta_lead_ads',
        'bandsintown_followers', 'google_customer_match', 'reddit_community'
    )),
    -- Pull-style sources are read from here by the generic HTTP-JSON adapter;
    -- push-style flows (n8n posting batches) may leave it NULL.
    fetch_url text CHECK (fetch_url IS NULL OR char_length(fetch_url) <= 512),
    -- Paid-ad fanbases milk a connected account: every lead the platform
    -- captured becomes a candidate fan routed through double-opt-in.
    connection_id uuid REFERENCES fanbase_connections(id) ON DELETE SET NULL,
    -- Operator attestation that the list was collected with consent, required
    -- for origins without a live connection and without per-candidate evidence.
    consent_attested_by text CHECK (consent_attested_by IS NULL OR (
        btrim(consent_attested_by) <> '' AND char_length(consent_attested_by) <= 200)),
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);

CREATE TABLE fanbase_ingestions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    fanbase_id uuid NOT NULL REFERENCES fanbases(id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    status text NOT NULL DEFAULT 'running'
        CHECK (status IN ('running', 'completed', 'failed')),
    received integer NOT NULL DEFAULT 0 CHECK (received >= 0),
    imported_pending integer NOT NULL DEFAULT 0 CHECK (imported_pending >= 0),
    confirmation_resent integer NOT NULL DEFAULT 0 CHECK (confirmation_resent >= 0),
    already_active integer NOT NULL DEFAULT 0 CHECK (already_active >= 0),
    skipped_suppressed integer NOT NULL DEFAULT 0 CHECK (skipped_suppressed >= 0),
    cooldown_skipped integer NOT NULL DEFAULT 0 CHECK (cooldown_skipped >= 0),
    invalid integer NOT NULL DEFAULT 0 CHECK (invalid >= 0),
    error text CHECK (error IS NULL OR char_length(error) <= 1000),
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);

CREATE INDEX fanbase_ingestions_fanbase_idx
    ON fanbase_ingestions (fanbase_id, started_at DESC);

CREATE TABLE fanbase_members (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    fanbase_id uuid NOT NULL REFERENCES fanbases(id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL REFERENCES fans(id) ON DELETE CASCADE,
    external_id text NOT NULL CHECK (btrim(external_id) <> '' AND char_length(external_id) <= 200),
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (fanbase_id, external_id)
);

CREATE INDEX fanbase_members_fan_idx
    ON fanbase_members (workspace_id, fan_id);
