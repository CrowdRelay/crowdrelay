-- The room the band was trying to fill.
--
-- `room_leak` — scans divided by room size, per show — is the plan's declared
-- master variable, and it had no denominator it could trust. The measurement
-- query derived room size from `sum(admission_pools.capacity)`, which is the
-- size of the *pass pools* issued for the night: guestlist, winners, comps.
-- A sold-out three-hundred-capacity room that issued twelve passes reported a
-- room of twelve, and a room that issued none reported nothing at all.
--
-- Those are different questions. How many passes were available is a
-- ticketing fact. How many people the room holds is a fact about the room,
-- known at the moment the show is booked and unchanged by anything ticketing
-- does afterwards. Deriving the second from the first is what made the master
-- variable unmeasurable on every show that ever happened.
--
-- Nullable on purpose. Backfilling a guess would be worse than the gap it
-- filled: a fabricated denominator produces a leak rate with a number's shape
-- and no meaning, and `Measure::Unmeasured` already says "no admission
-- capacity on record for this show" honestly. Shows booked before this
-- migration keep saying it until somebody who knows the room fills it in.

ALTER TABLE events
    ADD COLUMN room_capacity integer
        CHECK (room_capacity IS NULL OR (room_capacity > 0 AND room_capacity <= 1000000));

COMMENT ON COLUMN events.room_capacity IS
    'How many people the room holds. Not the pass-pool total: pools are a ticketing fact, this is a fact about the venue. NULL means unmeasured, never zero.';

-- Which negotiation produced this night.
--
-- The gig timeline starts at "Announced", T-21, and the work that got the
-- band the date — finding the room, proposing a window, the counter, the
-- acceptance — sat in a different context with no way back. An operator
-- reading the nine steps could not see what the show cost to win, and the
-- ladder could not show the two weeks before its own first rung.
--
-- Nullable because most shows predate the link, and because a show can always
-- be created without a negotiation behind it — a hometown gig nobody had to
-- win is still a show. `ON DELETE SET NULL`: losing the negotiation record
-- must never take the night with it.
ALTER TABLE events
    ADD COLUMN booking_opportunity_id uuid;

ALTER TABLE events
    ADD CONSTRAINT events_booking_opportunity_fk
        FOREIGN KEY (workspace_id, booking_opportunity_id)
        REFERENCES viryaos_team_opportunities (workspace_id, id)
        ON DELETE SET NULL;

-- One negotiation is one night. The partial unique index is what makes the
-- acceptance path idempotent at the schema rather than only in the statement
-- that writes it.
CREATE UNIQUE INDEX IF NOT EXISTS events_booking_opportunity_idx
    ON events (workspace_id, booking_opportunity_id)
    WHERE booking_opportunity_id IS NOT NULL;
