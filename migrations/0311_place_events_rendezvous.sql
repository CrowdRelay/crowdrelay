-- The show as rendezvous (§12-9, Sprint 4V.6b): one night at one room that
-- every tenant's event row can point at, plus what each side may publish into
-- the shared view.
--
-- place_events is global — the same shape the venue registry made work. "The
-- night at Klub X on the 14th" is one night no matter how many tenants hold
-- an event row for it, and no tenant owns it: nobody has to sign up for the
-- night to exist. Identity is (venue_id, event_date), the room's night keyed
-- on the event's starts_at date in UTC.
--
-- The link is derived, never asserted: an event joins a night only through
-- its place_venue_marks row, which is the single legitimate venue link the
-- registry already maintains. Two workspaces' shows at the same room on the
-- same date land on one place_events row without either tenant doing
-- anything.
--
-- place_event_contributions is the consent record — the
-- amplification_consents shape (0110): explicit, capped, revocable, audited.
-- Default contributes nothing. What an act publishes here is what a co-billed
-- act or a link-holding organiser may see; everything else stays home.
-- `terms` is the sharpest kind: the organiser reads only the SUM, never the
-- parts, so a band can prove the promoter cannot see its guarantee.
--
-- place_event_links is the organiser's scoped link — the listing share_token
-- pattern (0293): a rotatable bearer granting one lens over one night, minted
-- by a participating workspace, expiring on its own.
--
-- event_acts.confirmed_* is the other half of "the bill is a claim": a
-- tenant's event_acts row naming another workspace's act is that tenant's
-- assertion until the named act's workspace confirms it — exactly as
-- confirm_booking_candidate makes a route real. Confirmation only counts
-- when confirmed_by = act_workspace_id; the write path enforces it.

CREATE TABLE place_events (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    venue_id   uuid NOT NULL REFERENCES place_venues(id) ON DELETE CASCADE,
    -- The room's night: the event's starts_at date in UTC.
    event_date date NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (venue_id, event_date)
);

ALTER TABLE events
    ADD COLUMN place_event_id uuid REFERENCES place_events(id) ON DELETE SET NULL;
CREATE INDEX events_place_event_idx
    ON events (place_event_id) WHERE place_event_id IS NOT NULL;

CREATE TABLE place_event_contributions (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    place_event_id uuid NOT NULL REFERENCES place_events(id) ON DELETE CASCADE,
    workspace_id   uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    kind           text NOT NULL CHECK (kind IN
                     ('draw_estimate','announce_status','asks','terms')),
    value          jsonb NOT NULL,
    status         text NOT NULL DEFAULT 'active'
                     CHECK (status IN ('active','revoked')),
    created_at     timestamptz NOT NULL DEFAULT now(),
    revoked_at     timestamptz,
    revoke_reason  text CHECK (revoke_reason IS NULL OR char_length(revoke_reason) <= 1000),
    -- One contribution per kind per workspace per night. A re-contribute
    -- updates the row in place: the unique key covers revoked rows too, so
    -- the write path is an upsert, not an insert that a past revocation
    -- could block.
    UNIQUE (place_event_id, workspace_id, kind)
);

CREATE INDEX place_event_contributions_event_idx
    ON place_event_contributions (place_event_id) WHERE status = 'active';

CREATE TABLE place_event_links (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    place_event_id  uuid NOT NULL REFERENCES place_events(id) ON DELETE CASCADE,
    token           uuid NOT NULL DEFAULT gen_random_uuid() UNIQUE,
    lens            text NOT NULL CHECK (lens IN ('organiser')),
    created_by      uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    expires_at      timestamptz NOT NULL,
    revoked_at      timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- One live organiser link per night: re-minting revokes first, so the index
-- can only ever hold one row per place_event. Revoked rows stay — a sent
-- link's corpse is the audit that it existed.
CREATE UNIQUE INDEX place_event_links_active_uq
    ON place_event_links (place_event_id) WHERE revoked_at IS NULL;

ALTER TABLE event_acts
    ADD COLUMN confirmed_at timestamptz,
    ADD COLUMN confirmed_by uuid REFERENCES workspaces(id) ON DELETE SET NULL;
-- A half-written confirmation is worse than none.
ALTER TABLE event_acts
    ADD CHECK ((confirmed_at IS NULL) = (confirmed_by IS NULL));

-- The shared resolver: an event's night is its mark's venue plus the UTC
-- date of its starts_at. Called by both triggers; also the maintenance
-- entry point — a re-keyed show's old night loses its last event and is
-- deleted, because a place_events row no event points at is nobody's night.
CREATE FUNCTION place_events_link(event_uuid uuid) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
    venue_uuid uuid;
    night_date date;
    night uuid;
    previous uuid;
BEGIN
    SELECT mark.venue_id, (event.starts_at AT TIME ZONE 'UTC')::date
      INTO venue_uuid, night_date
      FROM place_venue_marks AS mark
      JOIN events AS event
        ON event.id = mark.event_id
     WHERE mark.event_id = event_uuid;
    -- No mark, no night — the mark is the only legitimate venue link.
    IF venue_uuid IS NULL THEN
        RETURN;
    END IF;

    SELECT place_event_id INTO previous FROM events WHERE id = event_uuid;

    INSERT INTO place_events (venue_id, event_date)
    VALUES (venue_uuid, night_date)
    ON CONFLICT (venue_id, event_date) DO NOTHING
    RETURNING id INTO night;
    IF night IS NULL THEN
        SELECT id INTO night
          FROM place_events
         WHERE venue_id = venue_uuid AND event_date = night_date;
    END IF;

    UPDATE events SET place_event_id = night WHERE id = event_uuid;

    IF previous IS NOT NULL AND previous IS DISTINCT FROM night THEN
        DELETE FROM place_events AS orphan
         WHERE orphan.id = previous
           AND NOT EXISTS (
               SELECT 1 FROM events AS event
                WHERE event.place_event_id = orphan.id
           );
    END IF;
END
$$;

-- A mark appearing (or being re-pointed by a venue rename) resolves the
-- event's night inside the same transaction as the claim that made it.
CREATE FUNCTION place_events_mark_link() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM place_events_link(NEW.event_id);
    RETURN NEW;
END
$$;

CREATE TRIGGER place_events_mark_link
AFTER INSERT OR UPDATE OF venue_id ON place_venue_marks
FOR EACH ROW EXECUTE FUNCTION place_events_mark_link();

-- A moved show re-keys the night. The mark still names the room; the new
-- starts_at names the date. No status arm: a cancelled show still happened
-- at the room's calendar, so the night keeps the row and the lenses filter
-- on status instead.
CREATE FUNCTION place_events_rekey() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM place_events_link(NEW.id);
    RETURN NEW;
END
$$;

CREATE TRIGGER place_events_rekey
AFTER UPDATE OF starts_at ON events
FOR EACH ROW EXECUTE FUNCTION place_events_rekey();

-- And when an event row itself goes away. place_venue_marks cascades, so the
-- mark is already gone by the time this fires — the night keeps only what
-- still points at it, and a night nothing points at is nobody's.
CREATE FUNCTION place_events_orphan_sweep() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.place_event_id IS NOT NULL THEN
        DELETE FROM place_events AS orphan
         WHERE orphan.id = OLD.place_event_id
           AND NOT EXISTS (
               SELECT 1 FROM events AS event
                WHERE event.place_event_id = orphan.id
           );
    END IF;
    RETURN OLD;
END
$$;

CREATE TRIGGER place_events_orphan_sweep
AFTER DELETE ON events
FOR EACH ROW EXECUTE FUNCTION place_events_orphan_sweep();

-- Backfill: every marked event already in the registry joins its night.
-- The same rule the triggers apply — mark first, then (venue, UTC date).
INSERT INTO place_events (venue_id, event_date)
SELECT DISTINCT mark.venue_id, (event.starts_at AT TIME ZONE 'UTC')::date
FROM place_venue_marks AS mark
JOIN events AS event
  ON event.id = mark.event_id
ON CONFLICT (venue_id, event_date) DO NOTHING;

UPDATE events AS event
SET place_event_id = night.id
FROM place_venue_marks AS mark
JOIN place_events AS night
  ON night.venue_id = mark.venue_id
WHERE mark.event_id = event.id
  AND night.event_date = (event.starts_at AT TIME ZONE 'UTC')::date;
