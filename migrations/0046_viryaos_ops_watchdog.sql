-- Native CrowdRelay operational watchdog state.
--
-- Queue/proof/executor health belongs to the Rust control plane. External
-- automation (n8n, Discord, email) is only a notification adapter for the
-- durable `viryaos.ops.status_changed` event emitted on incident transitions
-- and bounded reminders. This table stores only aggregate operational facts.

CREATE TABLE viryaos_ops_alert_state (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    alert_key text NOT NULL CHECK (btrim(alert_key) <> '' AND char_length(alert_key) <= 80),
    severity text NOT NULL CHECK (severity IN ('warning', 'critical')),
    summary text NOT NULL CHECK (btrim(summary) <> '' AND char_length(summary) <= 240),
    active boolean NOT NULL DEFAULT true,
    first_seen_at timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL,
    last_alerted_at timestamptz,
    recovered_at timestamptz,
    details jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(details) = 'object'),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, alert_key),
    CHECK (last_seen_at >= first_seen_at),
    CHECK ((active AND recovered_at IS NULL) OR (NOT active AND recovered_at IS NOT NULL))
);

CREATE TRIGGER viryaos_ops_alert_state_set_updated_at
BEFORE UPDATE ON viryaos_ops_alert_state
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_ops_alert_state_active_idx
    ON viryaos_ops_alert_state (workspace_id, severity, last_seen_at DESC, alert_key)
    WHERE active;
