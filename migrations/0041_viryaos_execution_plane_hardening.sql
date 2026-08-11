-- ViryaOS execution-plane hardening.
--
-- Provider-aware read models join emissions by action id. The original ledger
-- is keyed by emission_key for idempotent writes; this compact secondary index
-- keeps receipt/read-model lookups O(log n) without adding another state table.

CREATE INDEX IF NOT EXISTS viryaos_autopilot_action_emissions_action_idx
    ON viryaos_autopilot_action_emissions (workspace_id, action_id, emitted_at DESC);

-- Operator/Chief-of-Staff views read recent terminal actions by completion time.
-- A partial index avoids scanning historical action rows as the audit ledger grows.
CREATE INDEX IF NOT EXISTS viryaos_autopilot_actions_status_finished_idx
    ON viryaos_autopilot_actions (workspace_id, status, finished_at DESC, id DESC)
    WHERE status IN ('succeeded', 'failed');
