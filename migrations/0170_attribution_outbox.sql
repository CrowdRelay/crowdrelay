-- Attribution outbox: durable requests for credit allocation.
--
-- When a measurement completes, the brain enqueues an attribution
-- request here. An attribution worker picks it up, discovers competing
-- actions, runs the CreditAllocator, and writes the result to
-- viryaos_fan_credit_ledger. This decouples measurement completion
-- from the potentially expensive attribution computation.
--
-- The outbox gives durability and retry semantics: if the worker
-- crashes after the measurement transaction commits, the attribution
-- request is still pending and will be retried.
CREATE TABLE IF NOT EXISTS viryaos_attribution_requests (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid       NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    measurement_id uuid     NOT NULL,
    action_id   uuid        NOT NULL,
    attribution_version integer NOT NULL DEFAULT 1,
    status      text        NOT NULL DEFAULT 'pending',
    created_at  timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz,
    last_error  text,
    attempt_count integer   NOT NULL DEFAULT 0,
    CONSTRAINT attribution_status_valid CHECK (status IN ('pending', 'processing', 'done', 'failed')),
    CONSTRAINT attribution_version_positive CHECK (attribution_version >= 1)
);

-- Idempotent: one attribution result per (measurement_id, version).
CREATE UNIQUE INDEX IF NOT EXISTS idx_attribution_requests_idempotent
    ON viryaos_attribution_requests (measurement_id, attribution_version);

-- Fast claim query: pending requests ordered by creation time.
CREATE INDEX IF NOT EXISTS idx_attribution_requests_pending
    ON viryaos_attribution_requests (status, created_at)
    WHERE status = 'pending';
