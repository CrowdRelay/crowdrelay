-- Resolve provider-native identifiers (for example Gmail thread IDs) from the
-- existing immutable execution-receipt ledger. This replaces n8n workflow
-- static-data correlation without adding another persistence model.
CREATE INDEX IF NOT EXISTS viryaos_autopilot_execution_reports_provider_ref_idx
    ON viryaos_autopilot_execution_reports (
        workspace_id,
        executor_id,
        provider_reference,
        occurred_at DESC,
        id DESC
    )
    WHERE provider_reference IS NOT NULL AND status = 'succeeded';
