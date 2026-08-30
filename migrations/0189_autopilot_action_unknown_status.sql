-- Add 'unknown' status to autopilot_actions.
--
-- The 'unknown' status represents an execution outcome that cannot be
-- established — the intervention may have succeeded, but confirmation
-- was lost (e.g., worker crash during Reddit API call). This is NOT a
-- failure: the action ledger maps it to UNKNOWN, which triggers
-- reconciliation rather than treating it as a failed treatment.
--
-- The finished_at CHECK is updated: 'unknown' does NOT require
-- finished_at (it is not terminal — it can later resolve to 'succeeded'
-- or 'failed' via reconciliation).

ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_status_check;

ALTER TABLE viryaos_autopilot_actions
    ADD CONSTRAINT viryaos_autopilot_actions_status_check
    CHECK (status IN (
        'awaiting_approval', 'queued', 'processing',
        'succeeded', 'failed', 'cancelled', 'unknown'
    ));

-- Update the finished_at CHECK: 'unknown' is not terminal, so it does
-- not require finished_at. The old constraint required finished_at for
-- 'succeeded', 'failed', 'cancelled' — we keep that and exclude 'unknown'.
ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_check1;

ALTER TABLE viryaos_autopilot_actions
    ADD CONSTRAINT viryaos_autopilot_actions_check1
    CHECK ((status NOT IN ('succeeded', 'failed', 'cancelled')) OR finished_at IS NOT NULL);
