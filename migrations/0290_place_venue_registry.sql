-- Shared venue registry (§4f-2). A venue is the second shared object after
-- cities: tenants contribute observations, the registry aggregates across
-- them, and no venue ever has to sign up for its record to exist.
--
-- Two tables, deliberately different shapes:
--
-- place_venues is global. It carries no workspace_id — "Klub X in Wrocław"
-- is one room no matter how many tenants have played it. Identity is
-- (city_id, name_key): the same name in two cities is two rooms, and the
-- same room typed with different casing or spacing is one row.
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
    -- The 500 bound matches the event's own venue validation
    -- (domain::events validate_optional_text 500). Anything shorter would
    -- fail a legitimate event's write inside the trigger, and the
    -- workspace-scoped input is the only writer either way.
    name_key text NOT NULL CHECK (btrim(name_key) <> '' AND char_length(name_key) <= 500),
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 500),
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

-- The normalised identity of a room: case-insensitive, edge- and
-- internal-whitespace-insensitive. Two spellings of one room must land on
-- one venue row or the aggregate lies.
CREATE FUNCTION place_venue_key(venue_name text) RETURNS text
LANGUAGE sql IMMUTABLE AS $$
    SELECT nullif(regexp_replace(lower(btrim(venue_name)), '\s+', ' ', 'g'), '')
$$;

CREATE FUNCTION place_venues_mark_event() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    venue_uuid uuid;
    new_key text;
BEGIN
    -- workspace_id is not in the trigger's column list below because an
    -- event never changes workspace in practice — but if one ever does, the
    -- mark it leaves behind under the old workspace would double-count the
    -- room's record. Retract it before anything else.
    IF TG_OP = 'UPDATE'
       AND OLD.workspace_id IS DISTINCT FROM NEW.workspace_id THEN
        DELETE FROM place_venue_marks
        WHERE workspace_id = OLD.workspace_id AND event_id = OLD.id;
    END IF;

    -- A show that no longer names a room, lost its city, or left the show
    -- statuses retracts the claim it made. Only reached on UPDATE — an
    -- INSERT failing the gate simply never marked.
    new_key := place_venue_key(NEW.venue);
    -- An over-length venue name (raw writes can bypass the domain's 500-char
    -- validation) does not mark — a malformed name is not a room record, and
    -- a trigger must never be able to fail the event write that spawned it.
    IF new_key IS NULL
       OR char_length(new_key) > 500
       OR char_length(btrim(NEW.venue)) > 500
       OR NEW.city_id IS NULL
       OR NEW.status NOT IN ('published', 'completed') THEN
        IF TG_OP = 'UPDATE' THEN
            DELETE FROM place_venue_marks
            WHERE workspace_id = NEW.workspace_id AND event_id = NEW.id;
        END IF;
        RETURN NEW;
    END IF;

    INSERT INTO place_venues (city_id, name_key, display_name)
    VALUES (NEW.city_id, new_key, left(btrim(NEW.venue), 500))
    ON CONFLICT (city_id, name_key) DO NOTHING
    RETURNING id INTO venue_uuid;
    IF venue_uuid IS NULL THEN
        SELECT id INTO venue_uuid
        FROM place_venues
        WHERE city_id = NEW.city_id AND name_key = new_key;
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
AFTER INSERT OR UPDATE OF venue, city_id, status, workspace_id ON events
FOR EACH ROW EXECUTE FUNCTION place_venues_mark_event();

-- Backfill from the events that already exist so the registry answers on the
-- day it ships rather than the day the next show gets edited. Earliest-seen
-- casing wins the display name; event.id keeps the pick deterministic.
INSERT INTO place_venues (city_id, name_key, display_name)
SELECT DISTINCT ON (event.city_id, place_venue_key(event.venue))
    event.city_id, place_venue_key(event.venue), left(btrim(event.venue), 500)
FROM events AS event
WHERE place_venue_key(event.venue) IS NOT NULL
  AND event.city_id IS NOT NULL
  AND event.status IN ('published', 'completed')
ORDER BY event.city_id, place_venue_key(event.venue), event.created_at, event.id
ON CONFLICT (city_id, name_key) DO NOTHING;

INSERT INTO place_venue_marks (venue_id, workspace_id, event_id)
SELECT venue.id, event.workspace_id, event.id
FROM events AS event
JOIN place_venues AS venue
  ON venue.city_id = event.city_id
 AND venue.name_key = place_venue_key(event.venue)
WHERE place_venue_key(event.venue) IS NOT NULL
  AND event.city_id IS NOT NULL
  AND event.status IN ('published', 'completed')
ON CONFLICT (workspace_id, event_id) DO NOTHING;
