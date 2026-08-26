-- Label Portfolio Mode: one tenant operates an entire roster, and the roster's
-- audiences can amplify each other through explicit, revocable, capped
-- consent edges.
--
-- Why this is the commercial core: a point tool sells per artist; a platform
-- that owns every roster's fan graph can route a new signing in front of the
-- label's proven audiences — the one capability competitors without the data
-- cannot copy. The consent edge keeps it honest: fans never leave their home
-- workspace, deliveries enqueue through the home workspace's own outbox, and
-- every edge carries a purpose, a monthly cap and a cooldown.

CREATE TABLE organizations (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    name text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 200),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE workspaces
    ADD COLUMN organization_id uuid REFERENCES organizations(id) ON DELETE SET NULL;

CREATE INDEX workspaces_organization_idx ON workspaces (organization_id);

CREATE TABLE amplification_consents (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- The audience owner and the beneficiary are both roster members; the
    -- direction matters: fans stay in from_workspace, messages about the
    -- beneficiary's releases go out through from_workspace's own channels.
    from_workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    to_workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    CHECK (from_workspace_id <> to_workspace_id),
    purpose text NOT NULL CHECK (purpose IN (
        'cross_promote', 'release_feature', 'event_crossbill'
    )),
    scope text NOT NULL DEFAULT 'all_active'
        CHECK (scope IN ('all_active', 'double_opt_in')),
    status text NOT NULL DEFAULT 'proposed'
        CHECK (status IN ('proposed', 'active', 'paused', 'revoked')),
    max_campaigns_per_month smallint NOT NULL DEFAULT 2
        CHECK (max_campaigns_per_month BETWEEN 1 AND 12),
    cooldown_days smallint NOT NULL DEFAULT 21
        CHECK (cooldown_days BETWEEN 1 AND 120),
    approved_by text CHECK (approved_by IS NULL OR (
        btrim(approved_by) <> '' AND char_length(approved_by) <= 200)),
    approved_at timestamptz,
    revoked_at timestamptz,
    revoke_reason text CHECK (revoke_reason IS NULL OR char_length(revoke_reason) <= 1000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (from_workspace_id, to_workspace_id, purpose)
);

CREATE INDEX amplification_consents_org_idx
    ON amplification_consents (organization_id, status);

CREATE TABLE amplification_deliveries (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    consent_id uuid NOT NULL REFERENCES amplification_consents(id) ON DELETE CASCADE,
    from_workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    to_workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    campaign_reference text NOT NULL CHECK (
        btrim(campaign_reference) <> '' AND char_length(campaign_reference) <= 200),
    delivered_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (consent_id, fan_id, campaign_reference)
);

CREATE INDEX amplification_deliveries_cap_idx
    ON amplification_deliveries (consent_id, delivered_at DESC);
