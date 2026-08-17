-- Durable, consent-bounded post-delivery activation for physical Latarnik releases.
--
-- Delivery only schedules a due time. The worker re-checks the Beacon profile,
-- release topic and DNC/contactability at send time, then atomically marks the
-- follow-up queued or suppressed. Shipment PII never enters this flow.

ALTER TABLE viryaos_beacon_release_recipients
    ADD COLUMN activation_due_at timestamptz,
    ADD COLUMN activation_queued_at timestamptz,
    ADD COLUMN activation_suppressed_at timestamptz;

ALTER TABLE viryaos_beacon_release_recipients
    ADD CONSTRAINT viryaos_beacon_release_activation_state_check
    CHECK (
        (activation_due_at IS NULL AND activation_queued_at IS NULL AND activation_suppressed_at IS NULL)
        OR (
            status = 'delivered'
            AND activation_due_at IS NOT NULL
            AND (activation_queued_at IS NULL OR activation_queued_at >= activation_due_at)
            AND (activation_suppressed_at IS NULL OR activation_suppressed_at >= activation_due_at)
            AND NOT (activation_queued_at IS NOT NULL AND activation_suppressed_at IS NOT NULL)
        )
    );

CREATE INDEX viryaos_beacon_release_activation_due_idx
    ON viryaos_beacon_release_recipients
       (activation_due_at, campaign_id, beacon_id)
    WHERE status = 'delivered'
      AND activation_due_at IS NOT NULL
      AND activation_queued_at IS NULL
      AND activation_suppressed_at IS NULL;

COMMENT ON COLUMN viryaos_beacon_release_recipients.activation_due_at IS
    'Earliest activation follow-up time; worker re-checks Beacon consent/contactability at send time.';
