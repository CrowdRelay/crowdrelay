-- Audience Graph: durable prospecting map of the places where scene
-- audiences actually gather (subreddits, Discords, forums, playlists,
-- festivals, zines) plus the compliance rules of each place and the outreach
-- pipeline state machine that turns a discovery into a relationship.
--
-- Layering with the rest of the brain:
--   * Outreach Supply asks for discovery sweeps; this schema is the supply.
--   * Beacon / Outreach consume places through the same autonomy funnel as
--     every other action: evidence raises confidence, rules gate delivery,
--     the operator stays behind approval where contracts or money appear.
-- Raw scan payloads land in discovery_place_evidence verbatim, so ingestion
-- can be replayed and audited without re-crawling.

CREATE TABLE discovery_places (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    place_kind text NOT NULL CHECK (place_kind IN (
        'subreddit', 'discord', 'forum', 'facebook_group',
        'instagram', 'tiktok', 'youtube', 'playlist', 'zine', 'festival', 'other'
    )),
    platform text NOT NULL CHECK (btrim(platform) <> '' AND char_length(platform) <= 64),
    name text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 200),
    url text NOT NULL CHECK (char_length(url) <= 512),
    country_code char(2) CHECK (country_code ~ '^[A-Z]{2}$'),
    language char(2) CHECK (language ~ '^[a-z]{2}$'),
    genres text[] NOT NULL DEFAULT '{}',
    member_count integer CHECK (member_count >= 0),
    activity_bp integer CHECK (activity_bp BETWEEN 0 AND 10000),
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'archived', 'blocked')),
    notes text CHECK (notes IS NULL OR char_length(notes) <= 4000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, platform, url)
);

CREATE INDEX discovery_places_workspace_kind_idx
    ON discovery_places (workspace_id, place_kind, status);
CREATE INDEX discovery_places_genres_idx
    ON discovery_places USING gin (genres);

CREATE TABLE discovery_place_rules (
    place_id uuid PRIMARY KEY REFERENCES discovery_places(id) ON DELETE CASCADE,
    self_promo_ratio_percent smallint
        CHECK (self_promo_ratio_percent IS NULL OR self_promo_ratio_percent BETWEEN 0 AND 100),
    contact_channel text CHECK (contact_channel IS NULL OR (
        btrim(contact_channel) <> '' AND char_length(contact_channel) <= 64)),
    contact_target text CHECK (contact_target IS NULL OR (
        btrim(contact_target) <> '' AND char_length(contact_target) <= 200)),
    requires_approval boolean NOT NULL DEFAULT false,
    cooldown_days smallint NOT NULL DEFAULT 14
        CHECK (cooldown_days BETWEEN 1 AND 365),
    rules_summary text CHECK (rules_summary IS NULL OR char_length(rules_summary) <= 4000),
    verified_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE discovery_place_evidence (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    place_id uuid NOT NULL REFERENCES discovery_places(id) ON DELETE CASCADE,
    evidence_kind text NOT NULL CHECK (evidence_kind IN (
        'scan', 'mention', 'sample_post', 'mod_contact', 'manual_note'
    )),
    method text NOT NULL CHECK (btrim(method) <> '' AND char_length(method) <= 64),
    confidence_bp integer NOT NULL CHECK (confidence_bp BETWEEN 0 AND 10000),
    payload jsonb NOT NULL DEFAULT '{}'::jsonb,
    observed_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX discovery_place_evidence_place_idx
    ON discovery_place_evidence (place_id, observed_at DESC);
-- Retention-friendly: raw scan payloads are the bulk of the table.
CREATE INDEX discovery_place_evidence_scan_cleanup_idx
    ON discovery_place_evidence (observed_at, id)
    WHERE evidence_kind = 'scan';

CREATE TABLE discovery_outreach (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    place_id uuid NOT NULL UNIQUE REFERENCES discovery_places(id) ON DELETE CASCADE,
    stage text NOT NULL DEFAULT 'discovered'
        CHECK (stage IN (
            'discovered', 'researched', 'contacted', 'replied',
            'negotiating', 'partnered', 'declined', 'dormant'
        )),
    campaign_context text CHECK (campaign_context IS NULL OR (
        btrim(campaign_context) <> '' AND char_length(campaign_context) <= 200)),
    last_action_at timestamptz,
    next_eligible_at timestamptz NOT NULL DEFAULT now(),
    outcome_notes text CHECK (outcome_notes IS NULL OR char_length(outcome_notes) <= 4000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX discovery_outreach_due_idx
    ON discovery_outreach (workspace_id, stage, next_eligible_at)
    WHERE stage IN ('researched', 'replied', 'negotiating');
