-- Extend the growth_metric_sync NOTIFY trigger to also fire on Spotify and
-- Reddit connections, not just YouTube.
--
-- The growth metric sync worker now handles three platforms:
--   - YouTube: subscriber counts via Data API v3 (API key)
--   - Spotify: artist follower counts via Web API (client credentials)
--   - Reddit: subreddit subscriber counts via public JSON (no auth)
--
-- Reddit connections feed the "social" coverage bucket: the MetricPlatform
-- enum has no "reddit" variant, so the worker records Reddit subreddit
-- counts under platform='social' in viryaos_growth_metric_series. The
-- fanbase_connection platform stays 'reddit' (the connectable platform),
-- while the metric series platform is 'social' (the coverage bucket).

DROP TRIGGER IF EXISTS fanbase_connections_growth_metric_sync ON fanbase_connections;
DROP FUNCTION IF EXISTS notify_growth_metric_sync();

CREATE OR REPLACE FUNCTION notify_growth_metric_sync()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.platform IN ('youtube', 'spotify', 'reddit')
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

CREATE TRIGGER fanbase_connections_growth_metric_sync
    AFTER INSERT OR UPDATE OF status, provider_account_id
    ON fanbase_connections
    FOR EACH ROW
    EXECUTE FUNCTION notify_growth_metric_sync();
