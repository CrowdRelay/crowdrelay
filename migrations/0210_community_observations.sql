-- Structured community intelligence observations. Each row is one snapshot
-- of a community's measurable facts at a point in time. Raw scan payloads
-- still land in discovery_place_evidence; this table holds the extracted
-- measurements and entities that the Brain can reason over.
--
-- DESIGN RULE: this table holds FACTS, not INTERPRETATIONS.
-- Sentiment, audience_affinity, promotion_norm, and trend are
-- interpretations that belong in a future CommunitySignal layer, not here.
-- Promotion policy lives in discovery_place_rules, not here.
CREATE TABLE community_observations (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    place_id uuid NOT NULL REFERENCES discovery_places(id) ON DELETE CASCADE,
    observed_at timestamptz NOT NULL DEFAULT now(),

    -- Provenance: where did this observation come from?
    source text NOT NULL CHECK (btrim(source) <> '' AND char_length(source) <= 64),
    source_url text NOT NULL CHECK (char_length(source_url) <= 512),
    collector_version text NOT NULL CHECK (btrim(collector_version) <> '' AND char_length(collector_version) <= 32),

    -- Normalized measurements (facts, not interpretations)
    raw_activity_metrics jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- e.g. {"online_users": 59, "total_posts": 33515, "posts_last_24h": 12}

    -- Observation quality (how reliable is this measurement?)
    observation_quality integer NOT NULL CHECK (observation_quality BETWEEN 0 AND 10000),
    -- 0 = parser may have failed, 10000 = fully structured extraction

    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX community_observations_place_idx
    ON community_observations (place_id, observed_at DESC);
CREATE INDEX community_observations_workspace_idx
    ON community_observations (workspace_id, observed_at DESC);

-- Extracted entities (artists, topics, bands mentioned in the community).
-- Stored as structured rows, not text[], so they can later join to
-- fanbases, campaigns, and actions.
--
-- workspace_id and place_id are NOT denormalized here — they are derivable
-- from observation_id. Denormalizing them would create a data-integrity
-- risk (entity.workspace_id could disagree with observation.workspace_id).
-- Queries that need tenant scoping JOIN through community_observations.
CREATE TABLE community_entities (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    observation_id uuid NOT NULL REFERENCES community_observations(id) ON DELETE CASCADE,

    entity_type text NOT NULL CHECK (entity_type IN ('artist', 'band', 'topic', 'genre', 'label')),
    entity_ref text NOT NULL CHECK (btrim(entity_ref) <> '' AND char_length(entity_ref) <= 200),
    -- Normalized source-level identity (e.g. "Spiritbox", "djent", "Season of Mist").
    -- NOT a foreign key to fanbase/artist records — entity resolution is a
    -- future Sprint C concern.

    strength integer NOT NULL DEFAULT 0 CHECK (strength BETWEEN 0 AND 10000),
    -- Observed prominence only: mention count, thread count, section prominence.
    -- NOT relevance, affinity, influence, or recommendation score.

    observed_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX community_entities_observation_idx
    ON community_entities (observation_id, entity_type);
CREATE INDEX community_entities_entity_idx
    ON community_entities (entity_type, entity_ref);
