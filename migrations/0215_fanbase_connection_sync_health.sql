-- A connection that fails every sync must not report itself as healthy.
--
-- `fanbase_connections.status` records whether credentials were supplied, and
-- the console reads it as whether the channel works. Those came apart:
-- production carries five connections — discord, telegram, lastfm, facebook,
-- instagram — that have failed on every cycle since they were created, each
-- with a precise cause in the worker log:
--
--   discord   : disdex.io returned HTTP 404 for server 1074618080854032454
--   telegram  : Telegram Bot API returned HTTP 400 for @ViryaTY
--   lastfm    : Last.fm API key not configured (CROWDRELAY_LASTFM_API_KEY)
--   facebook  : Facebook Graph API returned HTTP 400 for page 101848539107631
--   instagram : Instagram Graph API returned HTTP 400 for ig_user 17841455886865962
--
-- All five say `connected`. The operator sees green over channels that have
-- never produced a single metric point, and the only place the truth exists is
-- a log line nobody reads.
--
-- `last_sync_at` already exists and has never been written — the one function
-- that sets it has no callers, so it is `NULL` for all 41 connections.
--
-- Two columns, not a status change: `status` keeps meaning "credentials are
-- present", which is what the connect flow sets and what the disconnect flow
-- clears. Sync health is a separate fact and belongs in its own field, so a
-- transient provider outage cannot look like a revoked credential.
ALTER TABLE fanbase_connections
    ADD COLUMN IF NOT EXISTS last_sync_error text
        CHECK (last_sync_error IS NULL OR char_length(last_sync_error) <= 500),
    ADD COLUMN IF NOT EXISTS last_sync_failed_at timestamptz;

COMMENT ON COLUMN fanbase_connections.last_sync_error IS
    'Why the most recent sync failed, verbatim from the provider adapter. NULL once a sync succeeds.';
COMMENT ON COLUMN fanbase_connections.last_sync_failed_at IS
    'When the most recent sync failed. Compare against last_sync_at to tell a channel that never worked from one that stopped.';

-- Finding the broken ones must not scan every connection of every workspace.
CREATE INDEX IF NOT EXISTS fanbase_connections_failing_idx
    ON fanbase_connections (workspace_id, platform)
    WHERE last_sync_error IS NOT NULL;
