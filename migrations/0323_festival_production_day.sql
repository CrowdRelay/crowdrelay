-- Bill-mates at festival scale (6.3): a festival slot projects a production
-- day of kind 'festival', but the dedupe index only covered 'show' — a gig
-- marked festival after projecting its day could mint a second one, and the
-- kind-flip sync then collided re-entering the index. The gig still gets
-- exactly one projected day; the predicate widens to both projected kinds.
-- Manually pinned days (a photoshoot on the gig) stay outside it as before.

-- If a hand-made 'festival' day already shares an event with a projected
-- 'show' day, fold the pair into the earliest row — plans and their
-- harvest move with it rather than dying with the duplicate.
WITH ranked AS (
    SELECT id,
           FIRST_VALUE(id) OVER w AS keep_id,
           ROW_NUMBER() OVER w AS position
    FROM viryaos_production_events
    WHERE event_id IS NOT NULL AND kind IN ('show', 'festival')
    WINDOW w AS (PARTITION BY workspace_id, event_id ORDER BY created_at, id)
)
UPDATE viryaos_capture_plans plan
SET production_event_id = ranked.keep_id,
    updated_at = now()
FROM ranked
WHERE plan.production_event_id = ranked.id
  AND ranked.position > 1;

WITH ranked AS (
    SELECT id,
           ROW_NUMBER() OVER (
               PARTITION BY workspace_id, event_id ORDER BY created_at, id
           ) AS position
    FROM viryaos_production_events
    WHERE event_id IS NOT NULL AND kind IN ('show', 'festival')
)
DELETE FROM viryaos_production_events day
USING ranked
WHERE day.id = ranked.id AND ranked.position > 1;

DROP INDEX viryaos_production_events_one_show_per_gig;

CREATE UNIQUE INDEX viryaos_production_events_one_show_per_gig
    ON viryaos_production_events (event_id)
    WHERE event_id IS NOT NULL AND kind IN ('show', 'festival');
