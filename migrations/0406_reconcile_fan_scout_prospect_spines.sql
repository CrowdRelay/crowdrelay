-- Reconcile the two FAN SCOUT prospect slices that landed concurrently.
--
-- 0405/person spine is the canonical active path: it has person ownership,
-- retention, privacy erasure, warm-comment ingestion and the funnel API.
-- 0404 tables are preserved under explicit legacy names so no accidental data
-- is destroyed, but application modules stop exporting/writing that path.
--
-- This migration also carries the one capability worth keeping from 0404:
-- stable provider/user/channel ids. Handles change; a stable platform id must
-- continue to resolve to the same person.

ALTER TABLE person_identities
    DROP CONSTRAINT IF EXISTS person_identities_kind_check,
    DROP CONSTRAINT IF EXISTS person_identities_value_check,
    DROP CONSTRAINT IF EXISTS person_identities_platform_iff_handle;

ALTER TABLE person_identities
    ADD CONSTRAINT person_identities_kind_check
        CHECK (kind IN ('email', 'platform_handle', 'platform_user_id')),
    ADD CONSTRAINT person_identities_value_check
        CHECK (
            btrim(value) <> ''
            AND (
                (kind IN ('email', 'platform_handle') AND value = lower(btrim(value)))
                OR
                (kind = 'platform_user_id' AND value = btrim(value))
            )
        ),
    ADD CONSTRAINT person_identities_platform_iff_public_identity
        CHECK (
            (kind IN ('platform_handle', 'platform_user_id'))
            = (platform IS NOT NULL)
        );

DROP INDEX IF EXISTS fan_prospects_one_per_handle;
CREATE UNIQUE INDEX fan_prospects_one_per_person_platform
    ON fan_prospects (workspace_id, person_id, platform);

ALTER TABLE scout_prospect_observations
    RENAME TO legacy_0404_scout_prospect_observations;
ALTER TABLE scout_prospects
    RENAME TO legacy_0404_scout_prospects;

ALTER INDEX IF EXISTS scout_prospect_observations_recent_idx
    RENAME TO legacy_0404_scout_prospect_observations_recent_idx;
ALTER INDEX IF EXISTS scout_prospects_status_idx
    RENAME TO legacy_0404_scout_prospects_status_idx;
ALTER INDEX IF EXISTS scout_prospects_linked_fan_idx
    RENAME TO legacy_0404_scout_prospects_linked_fan_idx;

COMMENT ON TABLE legacy_0404_scout_prospects IS
    'Deprecated FAN SCOUT #480 spine. Preserved for audit only; active code uses persons/fan_prospects.';
COMMENT ON TABLE legacy_0404_scout_prospect_observations IS
    'Deprecated FAN SCOUT #480 observations. Preserved for audit only; active code uses fan_prospect_observations.';
