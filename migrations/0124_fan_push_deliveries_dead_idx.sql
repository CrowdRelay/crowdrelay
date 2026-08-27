-- Partial index for the operator attention dead-push list.
-- The attention query filters on workspace_id + status IN ('failed','ambiguous')
-- and orders by created_at DESC, id DESC. Without this index the query does
-- a sequential scan on every attention call, growing with total push volume.

CREATE INDEX fan_push_deliveries_dead_idx
    ON fan_push_deliveries (workspace_id, created_at DESC, id DESC)
    WHERE status IN ('failed', 'ambiguous');
