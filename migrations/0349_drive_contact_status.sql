-- The verification sheet's liveness verdict survives staging.
--
-- The standing research loop verifies booking agents the way it verifies
-- venues and bands: a Status column carries `active`/`inactive` through
-- extraction into staging, and the sync's verdict pass drives the
-- matching `booking_agents.active` flag both ways — an `inactive` verdict
-- retires the row, an `active` one re-opens it, and no verdict at all
-- leaves the flag alone. What the sheet can never touch is
-- `refused_until` — that is the agent's own answer, not the sheet's to
-- reopen. An unrecognised verdict lands NULL: no claim, the same rule
-- the venue and band sheets follow.

ALTER TABLE drive_contacts
    ADD COLUMN IF NOT EXISTS staged_status text
    CHECK (staged_status IS NULL OR staged_status IN ('active', 'inactive'));
