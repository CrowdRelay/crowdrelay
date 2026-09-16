-- Shared venue registry (§4f-2). A venue is the second shared object after
-- cities: tenants contribute observations, the registry aggregates across
-- them, and no venue ever has to sign up for its record to exist.
--
-- Two tables, deliberately different shapes:
--
-- place_venues is global. It carries no workspace_id — "Klub X in Wrocław"
-- is one room no matter how many tenants have played it. Identity is
-- (city_id, name_key): the same name in two cities is two rooms, and the
-- same room typed with different casing is one row.
--
-- place_venue_marks is the contributed observation. It is workspace-scoped:
-- a mark says "this tenant's event happened at that room" and stays private
-- to its contributor. Tenants read the registry's aggregates — shows played,
-- typical draw, repeat attenders — never another tenant's mark rows.
--
-- The contribution source is events, so the record builds itself: every
-- published or completed event with a venue name and a city marks the room.
-- A trigger keeps marks honest — an event that loses its venue, its city,
-- or its show status retracts the claim it made. Draft and cancelled events
-- mark nothing: a planned night is not a played room.

CREATE TABLE place_venues (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    city_id uuid NOT NULL REFERENCES cities(id) ON DELETE RESTRICT,
    name_key text NOT NULL CHECK (btrim(name_key) <> '' AND char_length(name_key) <= 200),
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 200),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (city_id, name_key)
);

CREATE TABLE place_venue_marks (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    venue_id uuid NOT NULL REFERENCES place_venues(id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    marked_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, event_id)
);

CREATE INDEX place_venue_marks_venue_idx
    ON place_venue_marks (venue_id, workspace_id);

CREATE FUNCTION place_venues_mark_event() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    venue_uuid uuid;
BEGIN
    -- A show that no longer names a room, lost its city, or left the show
    -- statuses retracts the claim it made. Only reached on UPDATE — an
    -- INSERT failing the gate simply never marked.
    IF NEW.venue IS NULL OR btrim(NEW.venue) = ''
       OR NEW.city_id IS NULL
       OR NEW.status NOT IN ('published', 'completed') THEN
        IF TG_OP = 'UPDATE' THEN
            DELETE FROM place_venue_marks
            WHERE workspace_id = OLD.workspace_id AND event_id = OLD.id;
        END IF;
        RETURN NEW;
    END IF;

    INSERT INTO place_venues (city_id, name_key, display_name)
    VALUES (NEW.city_id, lower(btrim(NEW.venue)), btrim(NEW.venue))
    ON CONFLICT (city_id, name_key) DO NOTHING
    RETURNING id INTO venue_uuid;
    IF venue_uuid IS NULL THEN
        SELECT id INTO venue_uuid
        FROM place_venues
        WHERE city_id = NEW.city_id AND name_key = lower(btrim(NEW.venue));
    END IF;

    -- One mark per show per tenant. A venue rename re-points the mark rather
    -- than stacking a second claim on the same event.
    INSERT INTO place_venue_marks (venue_id, workspace_id, event_id)
    VALUES (venue_uuid, NEW.workspace_id, NEW.id)
    ON CONFLICT (workspace_id, event_id)
    DO UPDATE SET venue_id = EXCLUDED.venue_id;
    RETURN NEW;
END
$$;

CREATE TRIGGER place_venues_mark_event
AFTER INSERT OR UPDATE OF venue, city_id, status ON events
FOR EACH ROW EXECUTE FUNCTION place_venues_mark_event();

-- Backfill from the events that already exist so the registry answers on the
-- day it ships rather than the day the next show gets edited. Earliest-seen
-- casing wins the display name.
INSERT INTO place_venues (city_id, name_key, display_name)
SELECT DISTINCT ON (event.city_id, lower(btrim(event.venue)))
    event.city_id, lower(btrim(event.venue)), btrim(event.venue)
FROM events AS event
WHERE event.venue IS NOT NULL AND btrim(event.venue) <> ''
  AND event.city_id IS NOT NULL
  AND event.status IN ('published', 'completed')
ORDER BY event.city_id, lower(btrim(event.venue)), event.created_at
ON CONFLICT (city_id, name_key) DO NOTHING;

INSERT INTO place_venue_marks (venue_id, workspace_id, event_id)
SELECT venue.id, event.workspace_id, event.id
FROM events AS event
JOIN place_venues AS venue
  ON venue.city_id = event.city_id
 AND venue.name_key = lower(btrim(event.venue))
WHERE event.venue IS NOT NULL AND btrim(event.venue) <> ''
  AND event.city_id IS NOT NULL
  AND event.status IN ('published', 'completed')
ON CONFLICT (workspace_id, event_id) DO NOTHING;
