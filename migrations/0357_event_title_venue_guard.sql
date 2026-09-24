-- A venue field equal to the event's own title is the event's name, not a
-- room. Bandsintown fills `venue` with the tour/show title when the listing
-- carries no venue, and this trigger used to mint a `place_venues` row from
-- it verbatim — production grew phantom venues named "Sanity Check Tour"
-- and "WŁĄCZ SIĘ NA NOWE! NOOMN * VIRYA", each collecting marks and
-- place_events as if a room by that name existed.
--
-- The guard refuses only the *mint*: when a real venue row already bears
-- the key (a show literally named after the room it plays), the event still
-- links to it.

CREATE OR REPLACE FUNCTION place_venues_mark_event() RETURNS trigger
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

    -- A venue field carrying the event's own title is the event name, not a
    -- room — provider listings do this when the venue is unknown. Minting
    -- from it creates a phantom venue that collects marks and nights. Only
    -- the mint is refused: an existing venue row bearing the key still
    -- links, so a show genuinely named after its room keeps its mark.
    IF new_key = place_venue_key(NEW.title)
       AND NOT EXISTS (
           SELECT 1 FROM place_venues
           WHERE city_id = NEW.city_id AND name_key = new_key
       ) THEN
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
