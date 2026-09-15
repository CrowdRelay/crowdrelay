-- The T+7 post-show report payload is a durable business artifact: it is the
-- only copy of what the counterparty was mailed, and the control-plane
-- artifact view (`control_plane_event_report`) reads it back by the event id
-- embedded in the payload. Terminal outbox retention does not delete these
-- rows (see `delete_old_terminal_outbox_events` in the worker), so this
-- lookup is permanent — it gets its own partial index rather than scanning
-- the workspace's whole outbox on every page load. The index stays tiny:
-- one row per issued report.
CREATE INDEX outbox_events_report_artifact_idx
    ON outbox_events (workspace_id, (payload->>'event_id'), created_at DESC)
    WHERE event_type = 'crowdrelay.show.post_show_report_due';
