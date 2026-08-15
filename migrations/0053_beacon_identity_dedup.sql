-- Beacon identity must not collapse every email-less local amplifier of the
-- same kind into one row. Email is the strongest natural identity when known;
-- otherwise use the verified/public destination URL. Rows with neither remain
-- distinct by id and are ineligible for automatic email outreach anyway.
--
-- The original UNIQUE NULLS NOT DISTINCT constraint was auto-named by
-- PostgreSQL and its generated identifier can be truncated. Resolve it by its
-- definition instead of assuming a particular generated constraint name.
DO $$
DECLARE
    old_constraint text;
BEGIN
    SELECT con.conname
      INTO old_constraint
      FROM pg_constraint AS con
     WHERE con.conrelid = 'viryaos_beacons'::regclass
       AND con.contype = 'u'
       AND pg_get_constraintdef(con.oid) =
           'UNIQUE NULLS NOT DISTINCT (workspace_id, beacon_kind, city_id, contact_email)';

    IF old_constraint IS NOT NULL THEN
        EXECUTE format('ALTER TABLE viryaos_beacons DROP CONSTRAINT %I', old_constraint);
    END IF;
END
$$;

-- Old API versions allowed optional URLs containing only whitespace. Normalize
-- those legacy values before using destination_url as a fallback identity.
UPDATE viryaos_beacons
SET destination_url = NULLIF(btrim(destination_url), ''),
    source_url = NULLIF(btrim(source_url), '')
WHERE destination_url IS DISTINCT FROM NULLIF(btrim(destination_url), '')
   OR source_url IS DISTINCT FROM NULLIF(btrim(source_url), '');

CREATE UNIQUE INDEX viryaos_beacons_email_identity_uq
    ON viryaos_beacons (workspace_id, beacon_kind, city_id, contact_email) NULLS NOT DISTINCT
    WHERE contact_email IS NOT NULL;

CREATE UNIQUE INDEX viryaos_beacons_destination_identity_uq
    ON viryaos_beacons (workspace_id, beacon_kind, city_id, destination_url) NULLS NOT DISTINCT
    WHERE contact_email IS NULL AND destination_url IS NOT NULL;
