-- Festival slot is a show (6.1). The marker is one column: `festival_name`
-- present means this event is a slot inside a named festival — bigger room,
-- longer bill, and the whole Sprint 1G chain runs on it unmodified because
-- the chain already keys on status and date, not on what kind of night it
-- is.
--
-- The organiser rides `counterparty_name`/`counterparty_email` — "the
-- person on the other side of the show" is the festival organiser for a
-- slot, and the T+7 post-show report already delivers to them. A separate
-- organiser column would store the same fact twice.

ALTER TABLE events
    ADD COLUMN festival_name text
        CHECK (festival_name IS NULL
               OR (btrim(festival_name) <> '' AND char_length(festival_name) <= 200));

COMMENT ON COLUMN events.festival_name IS
    'The festival this show is a slot inside of. Its presence is what marks '
    'the event as a festival slot — NULL is an ordinary night. The organiser '
    'is carried by counterparty_name/counterparty_email so the post-show '
    'report reaches them unmodified.';
