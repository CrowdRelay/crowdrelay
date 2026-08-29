-- Growth metric sync: YouTube + Meta (FB/IG) reactive metric ingestion.
--
-- The Bandsintown tracker piggybacks on the event_sync cycle because
-- Bandsintown is already an event source. YouTube and Meta are not event
-- sources — they are social/platform surfaces where we track follower and
-- subscriber counts as growth metrics.
--
-- Design: reactive, not polling. The worker LISTENs on a Postgres NOTIFY
-- channel and wakes only when:
--   1. A new connection is created or transitions to 'connected' (trigger
--      below fires NOTIFY), or
--   2. The next scheduled sync time arrives (computed from the latest
--      recorded point's timestamp).
-- No ticker. No busy loop. No wake-without-work.
--
-- This migration:
-- 1. Adds `youtube` as a connectable fanbase platform.
-- 2. Adds `provider_account_id` to `fanbase_connections` — the platform
--    account identifier (YouTube channel ID, Meta page/IG business ID).
-- 3. Adds a trigger that fires NOTIFY on the `growth_metric_sync` channel
--    when a youtube/meta connection is created or transitions to connected.

-- 1. Add YouTube to the fanbase_connections platform check.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT fanbase_connections_platform_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform IN (
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown', 'spotify', 'youtube'
    ));

-- 2. Add YouTube to the fanbase_oauth_states platform check.
ALTER TABLE fanbase_oauth_states
    DROP CONSTRAINT fanbase_oauth_states_platform_check;
ALTER TABLE fanbase_oauth_states
    ADD CONSTRAINT fanbase_oauth_states_platform_check
    CHECK (platform IN (
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown', 'spotify', 'youtube'
    ));

-- 3. Provider account ID: the platform-specific identifier the metric sync
--    worker reads. For YouTube this is the channel ID (UC...). For Meta this
--    is the page ID or IG business account ID. Nullable because legacy
--    connections created before this column may not have it yet.
ALTER TABLE fanbase_connections
    ADD COLUMN IF NOT EXISTS provider_account_id text;

-- Index for the metric sync worker: find all active connections for a given
-- platform across all workspaces in one scan.
CREATE INDEX IF NOT EXISTS fanbase_connections_platform_active_idx
    ON fanbase_connections (platform, status)
    WHERE status = 'connected';

-- 4. NOTIFY trigger: fires when a youtube or meta connection is inserted or
--    transitions to 'connected'. The worker's PgListener wakes on this and
--    does an immediate sync of the new connection — no waiting for the next
--    scheduled cycle.
CREATE OR REPLACE FUNCTION notify_growth_metric_sync()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.platform IN ('youtube', 'meta')
       AND NEW.status = 'connected'
       AND (
           TG_OP = 'INSERT'
           OR OLD.status IS DISTINCT FROM NEW.status
           OR OLD.provider_account_id IS DISTINCT FROM NEW.provider_account_id
       )
    THEN
        PERFORM pg_notify('growth_metric_sync', NEW.platform);
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS fanbase_connections_growth_metric_sync ON fanbase_connections;
CREATE TRIGGER fanbase_connections_growth_metric_sync
    AFTER INSERT OR UPDATE OF status, provider_account_id
    ON fanbase_connections
    FOR EACH ROW
    EXECUTE FUNCTION notify_growth_metric_sync();
