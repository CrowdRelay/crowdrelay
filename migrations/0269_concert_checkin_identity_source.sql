-- A check-in proves a body in the room; how the fan behind it was identified
-- is a separate fact with a separate strength. A browser session is verified
-- identity. An email typed into the scan page is a claim until the inbox
-- confirms it. The T+7 report counts them separately, so the column is NOT
-- NULL with the honest default for every row that already exists.
ALTER TABLE concert_checkins
    ADD COLUMN identity_source text NOT NULL DEFAULT 'session'
        CHECK (identity_source IN ('session', 'email_claim'));
