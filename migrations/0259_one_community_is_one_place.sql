-- One community is one place.
--
-- `discovery_places` is unique on `(workspace_id, platform, url)`, so the URL is
-- the identity of a place, and discovery writes whatever spelling it found.
-- Measured in production 2026-09-13:
--
--   /r/MetalForTheMasses/        and  https://www.reddit.com/r/MetalForTheMasses
--   https://reddit.com/r/Djent   and  https://www.reddit.com/r/Djent
--
-- The cost is not an untidy table. One post is drafted per place, so production
-- held community post drafts for both `r/MetalMemes` and `r/metalmemes`, and for
-- both `r/listentothis` and `r/ListenToThis`. Reddit treats subreddit names
-- case-insensitively, so each pair is one community — and publishing both is
-- posting twice to the same place under the band's name, which is exactly the
-- spam the North Star rules out. `canonical_place_url` in
-- `crowdrelay_domain::audience_graph` stops new ones; this collapses the ones
-- already stored.
--
-- Deliberately only Reddit, and only a URL that *is* a subreddit. Folding by name
-- would have been wrong: production also holds `/r/InMetalWeTrust/` beside
-- `https://inmetalwetrust.club`, a subreddit and a website that share a name and
-- are two different places. A permalink is left alone too — a post inside a
-- subreddit is not the subreddit.

-- The canonical form, matching `canonical_place_url` exactly. An expression rather
-- than a stored column so there is one definition of "same place" per side and no
-- third state to drift.
CREATE OR REPLACE FUNCTION crowdrelay_canonical_place_url(url text)
RETURNS text
LANGUAGE sql
IMMUTABLE
PARALLEL SAFE
AS $$
    SELECT CASE
        WHEN name IS NULL THEN btrim(url)
        ELSE 'https://www.reddit.com/r/' || lower(name)
    END
    FROM (
        SELECT substring(
            btrim(url)
            FROM '^(?:https?://)?(?:www\.|old\.|new\.)?(?:reddit\.com)?/?r/([A-Za-z0-9_]{1,21})/?$'
        ) AS name
    ) AS matched;
$$;

-- Phase 1: merge the losers into the survivors.
--
-- Survivor per canonical URL: the most progressed membership first, because a
-- joined community is knowledge the duplicate does not have; then the row with
-- the most evidence behind it; then the oldest, so the choice is deterministic
-- and a re-run picks the same winner.

-- Repoint every dependent. Two of the five are unique per place, so the loser's
-- row is dropped when the survivor already has one rather than colliding.
CREATE TEMP TABLE crowdrelay_place_merges AS
WITH grouped AS (
    SELECT id,
           workspace_id,
           platform,
           crowdrelay_canonical_place_url(url) AS canonical,
           CASE membership_state
               WHEN 'joined' THEN 3
               WHEN 'joining' THEN 2
               WHEN 'blocked' THEN 1
               ELSE 0
           END AS progress,
           (SELECT count(*) FROM discovery_place_evidence e WHERE e.place_id = discovery_places.id) AS evidence,
           created_at
    FROM discovery_places
),
ranked AS (
    SELECT id,
           first_value(id) OVER (
               PARTITION BY workspace_id, platform, canonical
               ORDER BY progress DESC, evidence DESC, created_at ASC, id ASC
           ) AS survivor_id
    FROM grouped
)
SELECT id AS loser_id, survivor_id FROM ranked WHERE id <> survivor_id;

-- Two dependents are unique per place, and both had to be read off the live
-- schema rather than inferred: `discovery_place_rules.place_id` is that table's
-- primary key, and `discovery_outreach.place_id` carries its own unique index.
-- The first attempt at this migration guessed `agent_outreach_targets` instead of
-- `discovery_outreach` and failed on
-- `discovery_outreach_place_id_key` — harmlessly, because the deploy rolled the
-- whole migration back, but the lesson is that "which column is unique" is a
-- question for `pg_index`, not for reading a CREATE TABLE and assuming.
--
-- For both, the loser's row is dropped when the survivor already has one. The
-- survivor is the more progressed place by construction, so its row is the one
-- the loop has been reading and the one worth keeping.
DELETE FROM discovery_place_rules r
USING crowdrelay_place_merges m
WHERE r.place_id = m.loser_id
  AND EXISTS (SELECT 1 FROM discovery_place_rules s WHERE s.place_id = m.survivor_id);

UPDATE discovery_place_rules r
SET place_id = m.survivor_id
FROM crowdrelay_place_merges m
WHERE r.place_id = m.loser_id;

DELETE FROM discovery_outreach d
USING crowdrelay_place_merges m
WHERE d.place_id = m.loser_id
  AND EXISTS (SELECT 1 FROM discovery_outreach s WHERE s.place_id = m.survivor_id);

-- Not unique per place: `agent_outreach_targets` is unique on
-- (workspace_id, display_name, target_kind), so several targets may share a
-- place and all of them simply repoint.
UPDATE agent_outreach_targets t
SET place_id = m.survivor_id
FROM crowdrelay_place_merges m
WHERE t.place_id = m.loser_id;

UPDATE community_observations o
SET place_id = m.survivor_id
FROM crowdrelay_place_merges m
WHERE o.place_id = m.loser_id;

UPDATE discovery_outreach d
SET place_id = m.survivor_id
FROM crowdrelay_place_merges m
WHERE d.place_id = m.loser_id;

UPDATE discovery_place_evidence e
SET place_id = m.survivor_id
FROM crowdrelay_place_merges m
WHERE e.place_id = m.loser_id;

DELETE FROM discovery_places p
USING crowdrelay_place_merges m
WHERE p.id = m.loser_id;

DROP TABLE crowdrelay_place_merges;

-- Phase 2: canonicalise what survives. No collision is possible now, because
-- every group has exactly one row left.
UPDATE discovery_places
SET url = crowdrelay_canonical_place_url(url),
    updated_at = now()
WHERE url <> crowdrelay_canonical_place_url(url);

-- Phase 3: keep it true.
--
-- The unique constraint stays on `url` because that is what the upsert conflicts
-- on, and the application now writes the canonical form. This index is the net
-- under it: a row whose URL is not canonical cannot be inserted at all, so a
-- future call site that forgets `canonical_place_url` fails loudly here instead
-- of quietly creating the second copy of a community.
ALTER TABLE discovery_places
    ADD CONSTRAINT discovery_places_url_is_canonical
    CHECK (url = crowdrelay_canonical_place_url(url)) NOT VALID;

-- Validated separately so the ADD takes a weaker lock, and because phase 2 has
-- already made every existing row satisfy it.
ALTER TABLE discovery_places VALIDATE CONSTRAINT discovery_places_url_is_canonical;
