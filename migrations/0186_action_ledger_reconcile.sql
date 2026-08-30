-- Action Ledger reconciliation support.
--
-- This migration adds:
-- 1. A function to identify actions in UNKNOWN state that need reconciliation.
-- 2. A function to transition an action from RUNNING to UNKNOWN when it has
--    been running past its timeout (stale running detection).
-- 3. A function to attempt reconciliation of an UNKNOWN action.
--
-- The reconciliation strategy depends on the action kind:
-- - For community.engage: check if community_posts has a 'posted' row.
--   If yes → SUCCEEDED. If 'failed' → FAILED. If still 'posting' → stay UNKNOWN.
-- - For other action kinds: check if the action status in autopilot_actions
--   has been updated. If the action is 'succeeded' → SUCCEEDED. If 'failed' → FAILED.
--   If still 'processing' → stay UNKNOWN (will retry on next sweep).

-- Mark stale RUNNING actions as UNKNOWN.
-- Called by the worker reconciliation sweep.
CREATE OR REPLACE FUNCTION viryaos_action_ledger_mark_stale_unknown(
    p_workspace_id uuid,
    stale_threshold interval DEFAULT INTERVAL '10 minutes'
)
RETURNS TABLE (action_id uuid, workspace_id uuid)
LANGUAGE plpgsql
AS $$
BEGIN
    RETURN QUERY
    UPDATE viryaos_action_ledger
    SET state = 'UNKNOWN',
        state_entered_at = now(),
        updated_at = now(),
        transition_count = transition_count + 1,
        previous_state = 'RUNNING'
    WHERE viryaos_action_ledger.workspace_id = p_workspace_id
      AND state = 'RUNNING'
      AND state_entered_at < now() - stale_threshold
    RETURNING action_id, workspace_id;
END;
$$;

-- Reconcile a single UNKNOWN action.
-- Returns the new state ('SUCCEEDED', 'FAILED', or 'UNKNOWN' if still unresolved).
CREATE OR REPLACE FUNCTION viryaos_action_ledger_reconcile(
    target_action_id uuid
)
RETURNS text
LANGUAGE plpgsql
AS $$
DECLARE
    current_state text;
    action_status text;
    action_kind text;
    community_status text;
    new_state text;
BEGIN
    -- Lock the ledger row.
    SELECT state INTO current_state
    FROM viryaos_action_ledger
    WHERE action_id = target_action_id
    FOR UPDATE;

    -- Only reconcile UNKNOWN or RECONCILING actions.
    IF current_state IS NULL OR current_state NOT IN ('UNKNOWN', 'RECONCILING') THEN
        RETURN current_state;
    END IF;

    -- Transition to RECONCILING.
    IF current_state = 'UNKNOWN' THEN
        UPDATE viryaos_action_ledger
        SET state = 'RECONCILING',
            state_entered_at = now(),
            updated_at = now(),
            transition_count = transition_count + 1,
            previous_state = 'UNKNOWN',
            reconciliation_count = reconciliation_count + 1
        WHERE action_id = target_action_id;
    END IF;

    -- Get the action's current status and kind.
    SELECT status, action_kind INTO action_status, action_kind
    FROM viryaos_autopilot_actions
    WHERE id = target_action_id;

    -- Strategy 1: Check if the action status has been updated.
    IF action_status = 'succeeded' THEN
        new_state := 'SUCCEEDED';
    ELSIF action_status = 'failed' THEN
        new_state := 'FAILED';
    ELSIF action_status = 'cancelled' THEN
        new_state := 'CANCELLED';
    ELSIF action_kind = 'community.engage' THEN
        -- Strategy 2: For community engagement, check community_posts.
        SELECT status INTO community_status
        FROM community_posts
        WHERE action_id = target_action_id
        LIMIT 1;

        IF community_status = 'posted' THEN
            new_state := 'SUCCEEDED';
        ELSIF community_status = 'failed' THEN
            new_state := 'FAILED';
        ELSE
            -- Still pending/posting/rate_limited — remain UNKNOWN.
            new_state := 'UNKNOWN';
        END IF;
    ELSE
        -- No reconciliation strategy for this action kind — remain UNKNOWN.
        new_state := 'UNKNOWN';
    END IF;

    -- Apply the reconciliation result.
    IF new_state IN ('SUCCEEDED', 'FAILED', 'CANCELLED') THEN
        UPDATE viryaos_action_ledger
        SET state = new_state,
            state_entered_at = now(),
            updated_at = now(),
            transition_count = transition_count + 1,
            previous_state = 'RECONCILING'
        WHERE action_id = target_action_id;
    ELSE
        -- Still UNKNOWN — transition back from RECONCILING.
        UPDATE viryaos_action_ledger
        SET state = 'UNKNOWN',
            state_entered_at = now(),
            updated_at = now(),
            transition_count = transition_count + 1,
            previous_state = 'RECONCILING'
        WHERE action_id = target_action_id;
    END IF;

    RETURN new_state;
END;
$$;
