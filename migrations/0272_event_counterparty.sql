-- Event counterparty (Sprint 1G, 1G.12): the T+7 post-show report is delivered
-- to the band AND the promoter as an artifact email with no account required,
-- so the event needs to name the person on the other side of the show. This is
-- private contact data — it must never join the public event payload.
ALTER TABLE events
    ADD COLUMN counterparty_name text
        CHECK (counterparty_name IS NULL
               OR (btrim(counterparty_name) <> '' AND char_length(counterparty_name) <= 160)),
    ADD COLUMN counterparty_email text
        CHECK (counterparty_email IS NULL
               OR counterparty_email ~* '^[^@\s]+@[^@\s]+\.[^@\s]+$');
