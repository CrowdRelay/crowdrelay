-- What a connection is, next to what it does.
--
-- `fanbase_connections.status` does not mean what it reads like, and
-- `provider_verification.rs` says so in its own words: a connection whose
-- identity probe returns `Unavailable` -- network error, rate limit, missing
-- credential -- is stored as `connected`, because "we don't know yet". Nothing
-- revisits that guess.
--
-- The result is a column with four legal values that has exactly one value in
-- production. All 42 connections are `connected`, including 29 Reddit feeds
-- whose credential the agents service holds at `invalid` and which have never
-- been polled at all. A field that is constant carries no information, and this
-- one carries none while reading like a health check.
--
-- `status` is not changed. It is an honest statement of a different fact --
-- whether a credential is present -- and 17 call sites read it that way. What
-- was missing is the other fact, so this adds it rather than overloading the
-- first.
--
-- Generated, not written. There is no code path that can forget to update it,
-- no backfill that could guess wrong about rows created before it existed, and
-- no way for it to disagree with the two columns it derives from. The same rule
-- `/v1/admin/ops/connections` computes at read time, now available to anything
-- that needs it -- including the brain, which currently has no way to know that
-- a channel it is reasoning about has never worked.
--
--   failing     the last attempt failed
--   working     synced at least once and not currently failing
--   unverified  never synced and never failed -- no evidence either way, which
--               covers a platform the sync does not poll as well as one that
--               has simply not run yet. It does not mean broken.
ALTER TABLE fanbase_connections
    ADD COLUMN IF NOT EXISTS health text
    GENERATED ALWAYS AS (
        CASE
            WHEN last_sync_failed_at IS NOT NULL
                 AND (last_sync_at IS NULL OR last_sync_failed_at > last_sync_at)
                THEN 'failing'
            WHEN last_sync_at IS NOT NULL THEN 'working'
            ELSE 'unverified'
        END
    ) STORED;

-- The question anyone asks of this column is "which ones are not working", so
-- the index covers exactly that and leaves the healthy majority out of it.
CREATE INDEX IF NOT EXISTS fanbase_connections_unhealthy_idx
    ON fanbase_connections (workspace_id, platform)
    WHERE health <> 'working';
