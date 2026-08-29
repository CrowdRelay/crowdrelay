-- Add attribution provenance columns to the credit ledger.
-- These allow idempotent re-attribution: the same measurement can be
-- re-credited with a new attribution_version without duplicating rows.
ALTER TABLE viryaos_fan_credit_ledger
    ADD COLUMN IF NOT EXISTS measurement_id uuid,
    ADD COLUMN IF NOT EXISTS attribution_version integer NOT NULL DEFAULT 1;

-- Partial unique index: prevents duplicate credit rows for the same
-- measurement + version. NULL measurement_id rows are excluded (legacy
-- rows or manual entries without a measurement reference).
CREATE UNIQUE INDEX IF NOT EXISTS idx_credit_ledger_measurement_version
    ON viryaos_fan_credit_ledger (measurement_id, attribution_version)
    WHERE measurement_id IS NOT NULL;
