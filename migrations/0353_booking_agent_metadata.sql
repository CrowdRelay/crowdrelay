-- The registry workbook's booking-agent tab carries provenance the table
-- had no home for — Source_URL, Research_Date, Notes — and the direct seed
-- upsert needs somewhere to record which file a row came from. `metadata`
-- is the same object-typed bag `beacons` already uses for exactly this:
-- intake merges keys into it (`metadata || '{"imported_from": …}'`). The
-- merge is right-wins: `imported_from` is subtracted from the incoming
-- side so a row's first source stays history, while sheet-owned keys
-- (`source_url`, `research_date`, `notes`) follow the latest import.
--
-- Read-back columns stay read-back: `approached_at`, `refused_until`,
-- `do_not_contact` and `contact_verified_at` are the agent's or the
-- operator's answers and a sheet never writes them — the same contract
-- `promote_beacon_agent` already keeps.

ALTER TABLE booking_agents
    ADD COLUMN metadata jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(metadata) = 'object');
