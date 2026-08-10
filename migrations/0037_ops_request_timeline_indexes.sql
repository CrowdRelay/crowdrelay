-- Metadata-only operation timelines are queried by request/correlation id.
-- Keep these partial indexes scoped by workspace so diagnostics do not turn
-- into sequential scans as append-only audit/outbox history grows.
CREATE INDEX audit_events_ops_request_timeline_idx
    ON audit_events (workspace_id, request_id, occurred_at, id)
    WHERE request_id IS NOT NULL;

CREATE INDEX outbox_events_ops_request_timeline_idx
    ON outbox_events (workspace_id, request_id, created_at, id)
    WHERE request_id IS NOT NULL;

CREATE INDEX operator_actions_ops_request_timeline_idx
    ON operator_actions (workspace_id, request_id, created_at, id)
    WHERE request_id IS NOT NULL;
