-- Update the growth_metric_sync NOTIFY trigger and the platform active index
-- to match the new status filter (`status NOT IN ('invalid', 'expired')`).
--
-- 0236 added 'unverified' to the status vocabulary: a connection whose
-- creation-time probe returned Unavailable is now stored as 'unverified'
-- instead of 'connected'. The sync worker tries unverified connections and
-- promotes them to 'connected' on success.
--
-- The NOTIFY trigger (0213) only fired for `status = 'connected'`, so an
-- unverified connection would not wake the worker immediately. It would still
-- be picked up by the periodic cycle (FALLBACK_SLEEP = 5 minutes), but the
-- wake-on-connect contract was broken for the new status value.
--
-- The partial index (0172) only covered `status = 'connected'`, so the
-- worker's new `status NOT IN ('invalid', 'expired')` filter would not use
-- it for unverified or disconnected rows. Replace it with a non-partial index
-- that covers the worker's actual filter.

-- 1. NOTIFY trigger: fire for any status the sync worker considers due.
CREATE OR REPLACE FUNCTION notify_growth_metric_sync()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- No platform allowlist here on purpose: the worker's SYNCED_PLATFORMS is
    -- the one source of truth for what gets polled, and it filters the lease
    -- query. See scripts/test_platform_vocabulary_contract.py, which fails if
    -- an allowlist reappears in this function.
    --
    -- The status filter mirrors the worker's `status NOT IN ('invalid', 'expired')`
    -- so an unverified connection wakes the worker immediately on insert.
    IF NEW.status NOT IN ('invalid', 'expired')
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

-- 2. Replace the partial index with one that covers the worker's actual filter.
DROP INDEX IF EXISTS fanbase_connections_platform_active_idx;
CREATE INDEX IF NOT EXISTS fanbase_connections_platform_syncable_idx
    ON fanbase_connections (platform, status)
    WHERE status NOT IN ('invalid', 'expired');
