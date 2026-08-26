-- A verification clock for contact data, so `StaleContactData` growth debt
-- can fire.
--
-- `updated_at` is not a verification timestamp: it moves on any edit,
-- including a score change or a note, none of which means anybody confirmed
-- the contact route still works. `contact_verified_at` is set only when a
-- route is confirmed — a reply, a successful send, or an operator's explicit
-- verification — and it is the clock the growth-debt detector reads.
--
-- NULL means never verified, which is the correct idle signal for a target
-- that was imported but never tested: the detector reads it as stale from
-- `created_at`, which is a fact rather than an assumption.

ALTER TABLE viryaos_outreach_targets
    ADD COLUMN contact_verified_at timestamptz;

COMMENT ON COLUMN viryaos_outreach_targets.contact_verified_at IS
    'When the contact route was last confirmed working. NULL = never verified. Not updated by edits.';

ALTER TABLE viryaos_booking_targets
    ADD COLUMN contact_verified_at timestamptz;

COMMENT ON COLUMN viryaos_booking_targets.contact_verified_at IS
    'When the contact route was last confirmed working. NULL = never verified. Not updated by edits.';

-- Sorting and filtering by verification recency is the detector's whole
-- purpose. Partial indexes keep them small: most rows will be NULL until the
-- first wave or reply confirms them.
CREATE INDEX IF NOT EXISTS outreach_targets_verified_idx
    ON viryaos_outreach_targets (workspace_id, contact_verified_at DESC)
    WHERE contact_verified_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS booking_targets_verified_idx
    ON viryaos_booking_targets (workspace_id, contact_verified_at DESC)
    WHERE contact_verified_at IS NOT NULL;
