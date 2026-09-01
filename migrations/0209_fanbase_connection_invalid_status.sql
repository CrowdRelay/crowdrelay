-- Add 'invalid' to the fanbase_connections status CHECK constraint.
-- A provider-proven invalid identity (e.g. YouTube channel that doesn't
-- exist, Facebook page that returns 404) is stored with status='invalid'
-- so the growth metric sync worker skips it. The operator can delete the
-- invalid connection and create a correct one.
--
-- 'invalid' is a creation-time state: the ProviderVerifier proved the
-- external identity does not exist. It is NOT a runtime health state —
-- 'expired' and 'disconnected' remain the runtime lifecycle states set
-- by the sync worker.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_status_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_status_check
    CHECK (status IN ('connected', 'expired', 'disconnected', 'invalid'));
