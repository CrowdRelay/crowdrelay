-- Connection scan scope (1A.6, open decision 11).
--
-- The Drive and Gmail connectors read everything the account holds. The
-- extraction filter makes that safe in what is *kept*; it still means the
-- product reads everything a customer owns. Before a second tenant hands
-- over a login, the tenant chooses the boundary: a folder, a shared drive,
-- a label, a date — or the whole account, as a recorded choice rather than
-- a silent default.
--
-- `scan_scope` is a platform-shaped jsonb object; the domain validates its
-- contents, the column only promises an object or NULL. NULL means "not
-- chosen yet" — the sync cycle refuses to scan an unset connection and
-- says so, because an unchosen scope reading everything is exactly the
-- failure this column exists to prevent.
--
-- Existing gdrive/gmail rows already ran whole-account scans — the
-- operator who built the connector made that call. Backfilling them to
-- `whole_account` records the choice instead of changing behaviour under
-- their feet or pretending the question was never answered.

ALTER TABLE fanbase_connections
    ADD COLUMN IF NOT EXISTS scan_scope jsonb;

-- The column guards what it can: the platform↔kind pairing. Field shapes
-- (a folder_ids array, a parseable date) stay the domain's job — the CHECK
-- is the last line for a raw write that skips validation.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_scan_scope_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_scan_scope_check
    CHECK (
        scan_scope IS NULL
        OR (
            jsonb_typeof(scan_scope) = 'object'
            AND (
                (platform = 'gdrive'
                 AND scan_scope ->> 'kind' IN ('whole_account', 'folder', 'shared_drive', 'since'))
                OR (platform = 'gmail'
                 AND scan_scope ->> 'kind' IN ('whole_account', 'sent_only', 'label', 'since'))
            )
        )
    );

UPDATE fanbase_connections
SET scan_scope = '{"kind": "whole_account"}'::jsonb,
    updated_at = now()
WHERE platform IN ('gdrive', 'gmail')
  AND scan_scope IS NULL;
