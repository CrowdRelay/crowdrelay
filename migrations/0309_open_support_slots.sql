-- §12-5 / 4V.5: a confirmed show can say it has room on the bill.
--
-- The cheapest gig in the system is the one somebody else already booked: the
-- room is held, the promoter is committed, the date is set, and the only thing
-- missing is a name. `domain::roster_plan` has ranked a support-slot fill above
-- a new booking since it was written, and `open_slots` has arrived empty every
-- time, because nothing in the database could say a slot existed.
--
-- Declared, never inferred. "Two acts on the bill and a 300-capacity room, so
-- there is probably space" is the kind of guess that has the roster ask a
-- promoter for a slot they never offered — which costs the relationship the
-- proposal was supposed to build. The number is the band's or the manager's own
-- word, set when the promoter says so, and absent until then.
--
-- Zero and NULL are different answers and both are kept: NULL is "nobody has
-- said", zero is "we asked and the bill is full". The planner proposes on
-- neither, and only the second is a fact.

ALTER TABLE events
    ADD COLUMN open_support_slots smallint
        CHECK (open_support_slots IS NULL OR open_support_slots BETWEEN 0 AND 4);

COMMENT ON COLUMN events.open_support_slots IS
    'How many places on this bill the promoter has offered. NULL means nobody '
    'has said — never a claim that the bill is full, which is 0.';

-- The roster read asks for upcoming shows with room, across every act in one
-- organisation. Partial, because the rows with a slot are the rare ones.
CREATE INDEX events_open_support_slots_idx
    ON events (workspace_id, starts_at)
    WHERE open_support_slots IS NOT NULL AND open_support_slots > 0;
