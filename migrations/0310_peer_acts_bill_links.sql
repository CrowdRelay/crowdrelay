-- Peer acts and bill links (§12-5, 4V.6).
--
-- A name on a bill is evidence someone paid for the slot — the lineup is the
-- richest booking signal in the business, and until now it was a bare string
-- on one tenant's event row. This migration gives the string somewhere to
-- resolve to, in two directions.
--
-- place_peer_acts is global. It carries no workspace_id — "the band that
-- opened in Wrocław" is one band no matter whose bill it appeared on, exactly
-- as place_venues holds one room per room. Identity is the normalized name
-- key, plus an optional MusicBrainz artist id as the stronger anchor when a
-- dump import supplies one. Two acts may both lack an MBID; two acts may
-- never share one.
--
-- The link columns on event_acts are a resolution, nullable by design. A
-- billed name that resolves to a tenant workspace — by slug or by the act
-- name on its band listing — points at that tenant. Every other name points
-- at a peer act, minted on first sight. A resolution we cannot make is NULL,
-- never a guess: ON DELETE SET NULL means a removed workspace or peer leaves
-- the bill row with its name intact, which is correct — the name is the
-- record, the link is what we resolved about it.
--
-- place_genre_aliases is the canonical-form resolve map: free text stays the
-- display form, matching resolves through this table exactly as venue names
-- stay typed and resolve via name_key. It ships empty — a curated taxonomy
-- is MusicBrainz's job, and the dump importer that fills it is a later
-- sprint. The schema anchors the seam; it does not invent the vocabulary.

-- The normalized identity of a band that is not a tenant.
CREATE TABLE place_peer_acts (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name_key     text NOT NULL UNIQUE CHECK (btrim(name_key) <> '' AND char_length(name_key) <= 500),
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 500),
    mbid         uuid,          -- MusicBrainz artist id when a resolution anchor exists
    created_at   timestamptz NOT NULL DEFAULT now()
);
-- Two acts may both lack an MBID; two acts may never share one.
CREATE UNIQUE INDEX place_peer_acts_mbid_uq ON place_peer_acts (mbid) WHERE mbid IS NOT NULL;

-- Genre on a peer act, attributed: what the tag claims, who claimed it.
CREATE TABLE place_peer_act_genres (
    peer_act_id uuid NOT NULL REFERENCES place_peer_acts(id) ON DELETE CASCADE,
    genre_tag   text NOT NULL CHECK (btrim(genre_tag) <> '' AND char_length(genre_tag) <= 100),
    provenance  text NOT NULL CHECK (provenance IN
                  ('researched','event_evidence','open_directory','musicbrainz')),
    source_ref  text NOT NULL,
    observed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (peer_act_id, genre_tag, provenance, source_ref)
);

-- Canonical genre matching: free text is display, this is the resolve map.
CREATE TABLE place_genre_aliases (
    alias     text PRIMARY KEY,   -- already normalized (place_venue_key-normalized)
    canonical text NOT NULL CHECK (btrim(canonical) <> '' AND char_length(canonical) <= 100)
);

ALTER TABLE event_acts
    ADD COLUMN act_workspace_id uuid REFERENCES workspaces(id) ON DELETE SET NULL,
    ADD COLUMN peer_act_id      uuid REFERENCES place_peer_acts(id) ON DELETE SET NULL;
CREATE INDEX event_acts_peer_idx ON event_acts (peer_act_id) WHERE peer_act_id IS NOT NULL;
CREATE INDEX event_acts_ws_idx   ON event_acts (act_workspace_id) WHERE act_workspace_id IS NOT NULL;

-- Backfill. event_acts is empty in production today, so this is
-- correctness-by-construction rather than a data repair — the same rules the
-- write path applies, run once over real rows.
--
-- Peer mints first: every billed name the tenant resolution would leave
-- unlinked — no candidate, or candidates naming two different workspaces —
-- gets its global identity. `place_venue_key` is reused deliberately: it is
-- generic lower/trim/collapse normalization, not venue-specific. Earliest-seen
-- spelling wins the display name, as the venue backfill does it.
INSERT INTO place_peer_acts (name_key, display_name)
SELECT DISTINCT ON (place_venue_key(a.act_name))
    place_venue_key(a.act_name),
    left(btrim(a.act_name), 500)
FROM event_acts AS a
WHERE (
        SELECT count(DISTINCT candidate.workspace_id)
        FROM (
            SELECT w.id AS workspace_id
            FROM workspaces AS w
            WHERE w.slug = a.act_slug
            UNION ALL
            SELECT bl.workspace_id
            FROM viryaos_band_listings AS bl
            WHERE lower(btrim(bl.act_name)) = lower(btrim(a.act_name))
        ) AS candidate
    ) <> 1
ORDER BY place_venue_key(a.act_name), a.created_at, a.act_slug
ON CONFLICT (name_key) DO NOTHING;

-- The tenant link resolves only when every candidate names the same
-- workspace — a slug hit and a listing hit for different tenants is
-- ambiguous, and ambiguous resolves nothing.
UPDATE event_acts AS a
SET act_workspace_id = resolved.workspace_id
FROM (
    SELECT
        acts.workspace_id AS bill_workspace_id,
        acts.event_id,
        acts.act_slug,
        -- uuid has no ordering, so no min(): the HAVING guarantees one
        -- distinct value and [1] lifts it out of the aggregate.
        (array_agg(candidate.workspace_id))[1] AS workspace_id
    FROM event_acts AS acts
    CROSS JOIN LATERAL (
        SELECT w.id AS workspace_id
        FROM workspaces AS w
        WHERE w.slug = acts.act_slug
        UNION ALL
        SELECT bl.workspace_id
        FROM viryaos_band_listings AS bl
        WHERE lower(btrim(bl.act_name)) = lower(btrim(acts.act_name))
    ) AS candidate
    GROUP BY acts.workspace_id, acts.event_id, acts.act_slug
    HAVING count(DISTINCT candidate.workspace_id) = 1
) AS resolved
WHERE a.workspace_id = resolved.bill_workspace_id
  AND a.event_id = resolved.event_id
  AND a.act_slug = resolved.act_slug;

-- The peer link lands wherever the tenant link did not.
UPDATE event_acts AS a
SET peer_act_id = peer.id
FROM place_peer_acts AS peer
WHERE a.act_workspace_id IS NULL
  AND peer.name_key = place_venue_key(a.act_name);
