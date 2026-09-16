-- The room a tenant played and the room a tenant pitches are the same room.
--
-- Migration 0290 built the shared venue registry: `place_venues` is one row
-- per room, fed by every published or completed event, and `city_venues`
-- aggregates across tenants — shows played, typical draw, repeat attenders.
-- Migration 0033 built the booking pipeline, whose targets are workspace-owned
-- rows with a city, a display name and a contact.
--
-- Nothing joined them. A band could hold fourteen marks on Klub X and a booking
-- target called "Klub X" and the system had no way to notice they were the same
-- place, so every aggregate the registry computes was invisible to the pitch
-- that needed it. That is the join.
--
-- Three decisions worth stating, because each is easy to undo by accident:
--
-- **Only a venue gets a venue_id.** A promoter is a person who books rooms and
-- a festival is an event, not a room. Matching either by name against the
-- registry would attach a room record to something that is not one, and the
-- resulting "shows played" number would be nonsense nobody could trace back.
-- The CHECK enforces it rather than leaving it to the trigger's good behaviour.
--
-- **A missing link is NULL, never a guess.** A venue target whose room nobody
-- has played yet has no registry row, and the honest answer is "no history on
-- file" — the same rule as a missing read-model number being null and never 0.
-- Inventing a link to the nearest name would produce a confident wrong history,
-- which is worse than none.
--
-- **The link never creates a registry row.** `place_venues` is fed by shows
-- that actually happened; a pitch list is a list of rooms somebody hopes to
-- play. Letting a booking target mint a global identity row would fill the
-- shared registry with unverified names carrying no evidence, and the registry
-- exists precisely because its rows are backed by events. Rooms discovered
-- through research get in through the researched-facts path, which is a
-- separate decision with a provenance class attached (plan §12-2).

ALTER TABLE viryaos_booking_targets
    ADD COLUMN venue_id uuid REFERENCES place_venues(id) ON DELETE SET NULL;

-- A room leaving the registry must not take the booking relationship with it:
-- ON DELETE SET NULL above drops the link and keeps the target, its contact and
-- its history. The relationship is the tenant's; the room record is shared.

ALTER TABLE viryaos_booking_targets
    ADD CONSTRAINT viryaos_booking_targets_venue_link_is_a_venue
    CHECK (venue_id IS NULL OR target_kind = 'venue');

CREATE INDEX viryaos_booking_targets_venue_idx
    ON viryaos_booking_targets (venue_id)
    WHERE venue_id IS NOT NULL;

-- Resolution uses the registry's own key function, so a target typed with
-- different casing or doubled spaces lands on the same room the event trigger
-- would have marked. Anything else would produce two different answers to "is
-- this the same room" depending on which side asked.
CREATE FUNCTION viryaos_booking_targets_resolve_venue() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    resolved uuid;
BEGIN
    IF NEW.target_kind <> 'venue' THEN
        NEW.venue_id := NULL;
        RETURN NEW;
    END IF;

    SELECT venue.id INTO resolved
    FROM place_venues AS venue
    WHERE venue.city_id = NEW.city_id
      AND venue.name_key = place_venue_key(NEW.display_name);

    -- `resolved` is NULL when the room is not in the registry, and assigning
    -- NULL is the point: a target whose room was renamed out of a match loses
    -- the stale link rather than keeping a claim that no longer holds.
    NEW.venue_id := resolved;
    RETURN NEW;
END
$$;

CREATE TRIGGER viryaos_booking_targets_resolve_venue
BEFORE INSERT OR UPDATE OF display_name, city_id, target_kind
ON viryaos_booking_targets
FOR EACH ROW EXECUTE FUNCTION viryaos_booking_targets_resolve_venue();

-- The other direction: a room that enters the registry after the target was
-- created. Without this, a band that pitched Klub X in March and first played
-- it in June would still show no history, because the target row never changed.
CREATE FUNCTION place_venues_link_booking_targets() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE viryaos_booking_targets AS target
    SET venue_id = NEW.id
    WHERE target.target_kind = 'venue'
      AND target.city_id = NEW.city_id
      AND place_venue_key(target.display_name) = NEW.name_key
      AND target.venue_id IS DISTINCT FROM NEW.id;
    RETURN NEW;
END
$$;

CREATE TRIGGER place_venues_link_booking_targets
AFTER INSERT ON place_venues
FOR EACH ROW EXECUTE FUNCTION place_venues_link_booking_targets();

-- Backfill, so the link answers on the day it ships rather than the day the
-- next target gets edited. Same key function, same city scoping.
UPDATE viryaos_booking_targets AS target
SET venue_id = venue.id
FROM place_venues AS venue
WHERE target.target_kind = 'venue'
  AND venue.city_id = target.city_id
  AND venue.name_key = place_venue_key(target.display_name)
  AND target.venue_id IS NULL;
