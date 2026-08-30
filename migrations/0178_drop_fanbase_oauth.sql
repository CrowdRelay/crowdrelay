-- Drop OAuth infrastructure: token storage and OAuth state table.
--
-- Platform interactions now go through:
--   - The agents service browser (Reddit posting)
--   - API keys (YouTube subscriber metrics)
--   - n8n credential store (credential_ref pointer, unchanged)
--
-- crowdrelay's DB no longer stores OAuth tokens. The fanbase_oauth_states
-- table (PKCE verifiers, short-lived OAuth flow state) and the encrypted
-- token columns on fanbase_connections are no longer referenced by any
-- Rust code.

-- 1. Drop the OAuth state table entirely.
DROP TABLE IF EXISTS fanbase_oauth_states;

-- 2. Remove encrypted token columns from fanbase_connections.
--    These columns were added in migration 0126 and are now unused.
ALTER TABLE fanbase_connections
    DROP COLUMN IF EXISTS encrypted_access_token,
    DROP COLUMN IF EXISTS encrypted_refresh_token,
    DROP COLUMN IF EXISTS token_expires_at,
    DROP COLUMN IF EXISTS token_scope,
    DROP COLUMN IF EXISTS token_type,
    DROP COLUMN IF EXISTS account_name,
    DROP COLUMN IF EXISTS account_picture_url;

-- 3. Drop the social_posts table — the social_executor worker that used it
--    was dead code (declared as a module but never spawned in main.rs).
--    The RequestSocialPost action kind is also being removed from the brain.
DROP TABLE IF EXISTS social_posts;

-- 4. Update the growth_metric_sync trigger: only fire on YouTube connections.
--    Meta OAuth is gone — the worker only syncs YouTube subscriber counts via
--    the Data API v3 (API key, no OAuth). The old trigger fired on both
--    'youtube' and 'meta'; this replaces it with a YouTube-only version.
DROP TRIGGER IF EXISTS fanbase_connections_growth_metric_sync ON fanbase_connections;
DROP FUNCTION IF EXISTS notify_growth_metric_sync();

CREATE OR REPLACE FUNCTION notify_growth_metric_sync()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.platform = 'youtube'
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
