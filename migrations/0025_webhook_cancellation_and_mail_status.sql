-- Distinguish endpoint decommissioning from an exhausted delivery failure.
-- Historical endpoint_inactive rows are migrated out of the operator dead queue.

ALTER TABLE webhook_deliveries
    ADD COLUMN IF NOT EXISTS cancelled_at timestamptz;

DO $$
DECLARE
    constraint_name text;
BEGIN
    FOR constraint_name IN
        SELECT conname
        FROM pg_constraint
        WHERE conrelid = 'webhook_deliveries'::regclass
          AND contype = 'c'
          AND pg_get_constraintdef(oid) ILIKE '%status%pending%processing%delivered%dead%'
    LOOP
        EXECUTE format('ALTER TABLE webhook_deliveries DROP CONSTRAINT %I', constraint_name);
    END LOOP;
END
$$;

ALTER TABLE webhook_deliveries
    ADD CONSTRAINT webhook_deliveries_status_v2
    CHECK (status IN ('pending', 'processing', 'delivered', 'dead', 'cancelled'));

DO $$
DECLARE
    constraint_name text;
BEGIN
    FOR constraint_name IN
        SELECT conname
        FROM pg_constraint
        WHERE conrelid = 'webhook_delivery_attempts'::regclass
          AND contype = 'c'
          AND pg_get_constraintdef(oid) ILIKE '%outcome%delivered%retry%dead%'
    LOOP
        EXECUTE format('ALTER TABLE webhook_delivery_attempts DROP CONSTRAINT %I', constraint_name);
    END LOOP;
END
$$;

ALTER TABLE webhook_delivery_attempts
    ADD CONSTRAINT webhook_delivery_attempts_outcome_v2
    CHECK (outcome IN ('delivered', 'retry', 'dead', 'cancelled'));

UPDATE webhook_deliveries AS delivery
SET
    status = 'cancelled',
    cancelled_at = COALESCE(delivery.dead_at, delivery.updated_at, now()),
    dead_at = NULL
FROM webhook_endpoints AS endpoint
WHERE endpoint.workspace_id = delivery.workspace_id
  AND endpoint.id = delivery.endpoint_id
  AND NOT endpoint.active
  AND delivery.status = 'dead'
  AND delivery.last_error_kind = 'endpoint_inactive';

ALTER TABLE webhook_deliveries
    ADD CONSTRAINT webhook_deliveries_cancelled_at_v1
    CHECK ((status = 'cancelled') = (cancelled_at IS NOT NULL));

CREATE INDEX IF NOT EXISTS webhook_deliveries_cancelled_retention_idx
    ON webhook_deliveries (cancelled_at, id)
    WHERE status = 'cancelled';
