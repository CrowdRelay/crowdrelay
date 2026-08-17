-- Complete the durable show-growth measurement contract introduced in 0054.
-- A grassroots reply is intentionally explicit: sent/delivered/completed states do
-- not imply a human reply. Executors set reply_received=true in an activation
-- receipt and CrowdRelay records the first observed reply timestamp here.

ALTER TABLE viryaos_grassroots_activations
    ADD COLUMN reply_recorded_at timestamptz;

CREATE INDEX viryaos_grassroots_activations_reply_measurement_idx
    ON viryaos_grassroots_activations (workspace_id, event_id, reply_recorded_at)
    WHERE reply_recorded_at IS NOT NULL;
