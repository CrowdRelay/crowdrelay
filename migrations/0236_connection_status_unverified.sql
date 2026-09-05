-- Make fanbase_connections.status honest at write time.
--
-- A connection whose creation-time probe returned Unavailable (network error,
-- rate limit, missing credential) was stored as 'connected' — "we don't know
-- yet". Nothing revisits that guess, so 42 of 42 production connections read
-- 'connected', including 29 Reddit feeds whose credential is invalid and 1
-- Discogs connection that has never synced.
--
-- 'unverified' replaces 'connected' for that case. It means "we don't know
-- yet" — the credential might work, it might not. The sync worker tries
-- unverified connections; a successful sync promotes them to 'connected'.
--
-- 'connected' is reserved for connections that were Verified at creation time
-- or that have synced at least once. 'invalid' stays as it is: the provider
-- proved the identity is wrong. 'expired' stays: the sync worker sets it when
-- a token refresh fails.
--
-- The health generated column (migration 0235) derives runtime health from
-- sync history. status and health answer different questions: status is
-- credential state, health is sync state. Both are now honest.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_status_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_status_check
    CHECK (status IN ('connected', 'expired', 'disconnected', 'invalid', 'unverified'));

-- Backfill: connections that were stored as 'connected' but have never synced
-- and have a recorded failure are demoted to 'unverified'. The failure proves
-- the credential did not work; 'connected' was the optimistic guess that was
-- never revisited.
--
-- Connections that have never synced and have no failure stay 'connected' —
-- they were Verified at creation time and just have not been polled yet.
UPDATE fanbase_connections
SET status = 'unverified', updated_at = now()
WHERE status = 'connected'
  AND last_sync_at IS NULL
  AND last_sync_failed_at IS NOT NULL;
