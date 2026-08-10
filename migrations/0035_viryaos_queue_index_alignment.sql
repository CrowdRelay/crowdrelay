-- Align ViryaOS queue indexes with the actual workspace-scoped claim and
-- crash-recovery predicates. This keeps the hot polling paths bounded as
-- action and measurement history grows.

DROP INDEX IF EXISTS viryaos_autopilot_actions_due_idx;
CREATE INDEX viryaos_autopilot_actions_due_idx
    ON viryaos_autopilot_actions (workspace_id, available_at, id)
    WHERE status = 'queued' AND attempt_count < 5;

CREATE INDEX IF NOT EXISTS viryaos_autopilot_actions_processing_idx
    ON viryaos_autopilot_actions (workspace_id, started_at, id)
    WHERE status = 'processing';

DROP INDEX IF EXISTS viryaos_autopilot_measurements_due_idx;
CREATE INDEX viryaos_autopilot_measurements_due_idx
    ON viryaos_autopilot_measurements (workspace_id, due_at, available_at, id)
    WHERE status = 'pending' AND attempt_count < 3;

CREATE INDEX IF NOT EXISTS viryaos_autopilot_measurements_processing_idx
    ON viryaos_autopilot_measurements (workspace_id, started_at, id)
    WHERE status = 'processing';
