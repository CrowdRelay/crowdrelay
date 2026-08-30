-- Allow `webhook_delivery_attempts.outcome = 'ambiguous'`.
--
-- The outbox worker can now terminate a delivery as Ambiguous (e.g. the
-- provider returned a non-definitive response after exhausting retries, or
-- the request was sent but the response was lost). The ledger reconciliation
-- treats ambiguous attempts as authoritative evidence that the linked
-- autopilot action is UNKNOWN.
--
-- The previous constraint (webhook_delivery_attempts_outcome_v2, from
-- migration 0025) only allowed ('delivered', 'retry', 'dead', 'cancelled').

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
    ADD CONSTRAINT webhook_delivery_attempts_outcome_v3
    CHECK (outcome IN ('delivered', 'retry', 'dead', 'cancelled', 'ambiguous'));
