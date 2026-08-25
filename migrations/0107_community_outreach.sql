-- Weekly community outreach: researched target communities get assigned to
-- the social-skill team member with tracked smart links auto-generated.
CREATE TABLE viryaos_community_outreach_targets (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    symbol_slug text NOT NULL CHECK (btrim(symbol_slug) <> '' AND char_length(symbol_slug) <= 64),
    community_name text NOT NULL CHECK (btrim(community_name) <> '' AND char_length(community_name) <= 200),
    platform text NOT NULL CHECK (platform IN ('reddit','forum','discord','webzine','newsletter')),
    url text NOT NULL CHECK (url ~ '^https?://'),
    country_code char(2) NOT NULL CHECK (country_code ~ '^[A-Z]{2}$'),
    language text NOT NULL DEFAULT 'pl' CHECK (char_length(language) <= 8),
    post_template_key text NOT NULL DEFAULT 'community_outreach_v1',
    self_promo_policy text NOT NULL DEFAULT 'tolerant' CHECK (self_promo_policy IN ('tolerant','strict','megathread_only','prohibited')),
    priority integer NOT NULL DEFAULT 50 CHECK (priority BETWEEN 0 AND 100),
    active boolean NOT NULL DEFAULT true,
    last_assigned_at timestamptz,
    times_assigned integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, symbol_slug)
);

CREATE INDEX viryaos_community_outreach_due_idx
    ON viryaos_community_outreach_targets (workspace_id, active, priority DESC)
    WHERE active;

CREATE TABLE viryaos_community_outreach_assignments (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_id uuid NOT NULL REFERENCES viryaos_community_outreach_targets(id) ON DELETE CASCADE,
    assigned_to uuid,
    smart_link_id uuid,
    post_title text NOT NULL,
    post_body text NOT NULL,
    assigned_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    UNIQUE (workspace_id, target_id, assigned_at)
);
