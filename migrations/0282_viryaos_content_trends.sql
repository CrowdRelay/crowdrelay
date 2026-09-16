-- 3.5b.3 — detected content trends. Both observation tables (peer supply,
-- fan demand) are raw dated facts; this table holds the patterns the
-- deterministic detector distilled from them, with the evidence links that
-- make a trend auditable rather than a vibe.
--
-- One live row per (workspace, dimension, pattern): the sweep upserts on
-- every pass, so the row is always the latest reading, and `status` carries
-- the lifecycle (a pattern that stops appearing fades instead of vanishing,
-- which is itself a fact).

CREATE TABLE viryaos_content_trends (
    id                  UUID PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- One of the five dimensions the detector aggregates over.
    dimension           TEXT NOT NULL CHECK (dimension IN
                        ('format', 'theme', 'styling', 'timing', 'platform')),
    -- The pattern key inside the dimension: 'playthrough', 'youtube',
    -- 'friday'. Free text — the lexicons grow with what the sweep sees.
    pattern             TEXT NOT NULL CHECK (btrim(pattern) <> ''),
    -- 0..10000 bp — sources corroborating (60%) plus evidence volume (40%).
    strength            INTEGER NOT NULL CHECK (strength BETWEEN 0 AND 10000),
    -- Distinct origins the evidence came from (peer ids + place ids). A
    -- pattern one source repeats is weaker than one two sources agree on —
    -- that is the whole "strongest at ≥2 sources" rule.
    sources             INTEGER NOT NULL CHECK (sources >= 0),
    -- The rows that produced this reading, capped: {"peer": [ids], "fan": [ids]}.
    -- BIGINT ids — both observation tables use identity keys.
    evidence            JSONB NOT NULL DEFAULT '{}',
    -- Status lifecycle: 'emerging' (loud single source), 'confirmed'
    -- (≥2 distinct sources), 'faded' (was live, stopped appearing).
    status              TEXT NOT NULL DEFAULT 'emerging' CHECK (status IN
                        ('emerging', 'confirmed', 'faded')),
    first_seen          DATE NOT NULL,
    last_seen           DATE NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, dimension, pattern)
);
CREATE INDEX viryaos_content_trends_active_idx
    ON viryaos_content_trends (workspace_id, status, strength DESC);
