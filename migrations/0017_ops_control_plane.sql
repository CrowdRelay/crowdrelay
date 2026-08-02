CREATE TABLE operator_actions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action text NOT NULL CHECK (btrim(action) <> '' AND length(action) <= 64),
    target_type text NOT NULL CHECK (btrim(target_type) <> '' AND length(target_type) <= 64),
    target_id uuid NOT NULL,
    actor_type text NOT NULL DEFAULT 'admin_api_key'
        CHECK (actor_type IN ('admin_api_key')),
    idempotency_key text NOT NULL CHECK (idempotency_key ~ '^[!-~]{8,128}$'),
    request_id text CHECK (request_id IS NULL OR length(request_id) <= 128),
    details jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, idempotency_key)
);

CREATE INDEX operator_actions_target_idx
    ON operator_actions (workspace_id, target_type, target_id, created_at DESC);

CREATE INDEX operator_actions_created_idx
    ON operator_actions (workspace_id, created_at DESC, id DESC);

-- Worker claim indexes remain unchanged. These indexes are deliberately scoped
-- to operator reads, which always start with workspace_id.
CREATE INDEX outbox_events_ops_status_created_idx
    ON outbox_events (workspace_id, status, created_at DESC, id DESC);

CREATE INDEX webhook_deliveries_ops_status_created_idx
    ON webhook_deliveries (workspace_id, status, created_at DESC, id DESC);

-- The existing UNIQUE (workspace_id, delivery_id, attempt_number) index can
-- already be scanned backwards for attempt history; do not duplicate it.
CREATE INDEX outbox_events_ops_pending_age_idx
    ON outbox_events (workspace_id, available_at, id)
    WHERE status = 'pending';

CREATE INDEX webhook_deliveries_ops_pending_age_idx
    ON webhook_deliveries (workspace_id, available_at, id)
    WHERE status = 'pending';

CREATE TRIGGER operator_actions_append_only
BEFORE UPDATE OR DELETE ON operator_actions
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_append_only_mutation();
