-- A deleted event takes its content sources with it; a venue that repeats
-- the title is not a venue.
--
-- `project_event_content_sources` fires on INSERT and UPDATE of `events`, so
-- a DELETE left the event's `event` and `show_completed` sources active with
-- nothing behind them. On 2026-09-24 a sheet import created shows that
-- duplicated Bandsintown rows, and the duplicates were deleted by hand. Their
-- sources stayed live: the brain requested two listings each for Gorzów and
-- Namysłów, and kept promoting four past nights as upcoming — the past-show
-- gate in `load_content_supply_snapshots` joins the event row, and an orphan
-- has none. The cadence register counted the orphans as held moments too.
--
-- 1. An AFTER DELETE trigger retires both sources, the same way the
--    projection retires the `event` source when a show stops being published.
-- 2. Existing orphans are retired. Only projection-shaped rows are touched:
--    an `event` source whose own id is the event id it is keyed by, and a
--    `show_completed` source keyed by an event id. Both writers key them that
--    way; an operator-entered source keyed otherwise is left alone.
-- 3. A Bandsintown venue that only repeats the title is cleared. The Gorzów
--    show arrived as title "Sanity Check Tour", venue "Sanity Check Tour",
--    and letters named the tour as the room. The sync now drops such a venue
--    on arrival and no longer lets a missing one erase a recorded one.

CREATE OR REPLACE FUNCTION retire_deleted_event_content_sources()
 RETURNS trigger
 LANGUAGE plpgsql
AS $function$
BEGIN
    UPDATE content_sources
    SET active = false, version = version + 1
    WHERE workspace_id = OLD.workspace_id
      AND active
      AND ((source_kind = 'event' AND source_key = 'event:' || OLD.id::text)
        OR (source_kind = 'show_completed' AND source_key = 'show_completed:' || OLD.id::text));
    RETURN OLD;
END
$function$;

DROP TRIGGER IF EXISTS events_retire_deleted_content_sources ON events;
CREATE TRIGGER events_retire_deleted_content_sources
    AFTER DELETE ON events
    FOR EACH ROW EXECUTE FUNCTION retire_deleted_event_content_sources();

UPDATE content_sources AS source
SET active = false, version = source.version + 1
WHERE source.active
  AND (
      (source.source_kind = 'event'
       AND source.source_key = 'event:' || source.id::text)
      OR (source.source_kind = 'show_completed'
          AND source.source_key ~ '^show_completed:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$')
  )
  AND NOT EXISTS (
      SELECT 1 FROM events AS event
      WHERE event.workspace_id = source.workspace_id
        AND event.id::text = split_part(source.source_key, ':', 2)
  );

UPDATE events
SET venue = NULL
WHERE source_provider = 'bandsintown'
  AND venue IS NOT NULL
  AND lower(btrim(venue)) = lower(btrim(title));
