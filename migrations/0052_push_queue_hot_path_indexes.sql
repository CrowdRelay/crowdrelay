-- Push workers are workspace-scoped. Put workspace_id first so due/ack maintenance
-- can prune unrelated tenants before walking time-ordered partial indexes.
-- Reuse the existing index names to avoid extra write amplification.

DROP INDEX IF EXISTS fan_push_deliveries_due_idx;
CREATE INDEX fan_push_deliveries_due_idx
    ON fan_push_deliveries (workspace_id, available_at, created_at, id)
    WHERE status IN ('queued','retry_wait');

DROP INDEX IF EXISTS fan_push_deliveries_ack_idx;
CREATE INDEX fan_push_deliveries_ack_idx
    ON fan_push_deliveries (workspace_id, ack_deadline, id)
    WHERE status = 'provider_accepted';
