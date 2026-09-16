-- 3.5b.2 — fan-side observations. The peer-observation sweep records what
-- comparable acts publish; this table records what the fans in an admitted
-- community actually engage with — one dated row per post/thread the
-- community surfaced. Same grain, same dedup shape, so the trend detector
-- (3.5b.3) can read demand-side and supply-side facts in one pass.
--
-- discovery_places gains UNIQUE (workspace_id, id) so the composite foreign
-- key pins the place to its tenant — the same referential-integrity pattern
-- 0280 applied to the peer tables (0088 lineage).
ALTER TABLE discovery_places
    ADD CONSTRAINT discovery_places_workspace_id_key UNIQUE (workspace_id, id);

CREATE TABLE viryaos_fan_observations (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    place_id            UUID NOT NULL,
    -- The date the post happened when the source reports it, else the sweep
    -- day — "a top post today" is a fact dated by its observation.
    observed_at         DATE NOT NULL,
    captured_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Which surface produced it — 'reddit', 'brutalland', ...
    platform            TEXT NOT NULL CHECK (btrim(platform) <> ''),
    -- 'post', 'thread', 'comment_burst', ... — free text like its peer twin.
    kind                TEXT NOT NULL CHECK (btrim(kind) <> ''),
    -- The thing people engaged with: the post title, one line. Never a
    -- summary, never a vibe.
    fact                TEXT NOT NULL CHECK (btrim(fact) <> ''),
    url                 TEXT,
    -- Engagement as the source reported it — {"score": 412, "comments": 96}.
    metrics             JSONB NOT NULL DEFAULT '{}',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (workspace_id, place_id)
        REFERENCES discovery_places (workspace_id, id) ON DELETE CASCADE
);
CREATE INDEX viryaos_fan_observations_tail_idx
    ON viryaos_fan_observations (workspace_id, place_id, observed_at DESC);
-- A post that stays hot for a week reappears in every sweep; the dedup key
-- freezes the first sighting rather than re-recording drift. Platform is
-- part of the key for the same reason it is on the peer table.
CREATE UNIQUE INDEX viryaos_fan_observations_dedup_idx
    ON viryaos_fan_observations
    (workspace_id, place_id, observed_at, platform, kind, md5(fact));
