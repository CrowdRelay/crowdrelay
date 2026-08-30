-- Update the action ledger trigger to map 'unknown' → 'UNKNOWN'.
--
-- The state machine already allows RUNNING → UNKNOWN. This migration
-- updates the trigger function to recognize the new 'unknown' action
-- status and map it to the UNKNOWN ledger state.
--
-- The trigger itself does not need recreation — it references the
-- function by name and picks up the new body automatically.

CREATE OR REPLACE FUNCTION viryaos_action_ledger_sync()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    new_ledger_state text;
    current_ledger_state text;
BEGIN
    -- Map the action status to the ledger state.
    new_ledger_state := CASE NEW.status
        WHEN 'awaiting_approval' THEN 'AUTHORIZED'
        WHEN 'queued' THEN 'QUEUED'
        WHEN 'processing' THEN 'RUNNING'
        WHEN 'succeeded' THEN 'SUCCEEDED'
        WHEN 'failed' THEN 'FAILED'
        WHEN 'cancelled' THEN 'CANCELLED'
        WHEN 'unknown' THEN 'UNKNOWN'
        ELSE NULL
    END;

    -- If the status doesn't map to a ledger state, skip.
    IF new_ledger_state IS NULL THEN
        RETURN NEW;
    END IF;

    -- Check if a ledger entry already exists.
    SELECT state INTO current_ledger_state
    FROM viryaos_action_ledger
    WHERE action_id = NEW.id
    FOR UPDATE;

    IF current_ledger_state IS NULL THEN
        -- Insert a new ledger entry.
        INSERT INTO viryaos_action_ledger
            (action_id, workspace_id, state, trace_id, decision_id,
             state_entered_at, transition_count, previous_state)
        VALUES
            (NEW.id, NEW.workspace_id, new_ledger_state, NEW.trace_id,
             NEW.decision_id, now(), 0, NULL);
    ELSIF current_ledger_state = new_ledger_state THEN
        -- No state change — just update the trace_id if it was NULL.
        UPDATE viryaos_action_ledger
        SET trace_id = COALESCE(viryaos_action_ledger.trace_id, NEW.trace_id),
            updated_at = now()
        WHERE action_id = NEW.id;
    ELSE
        -- State transition — enforce the monotonic state machine.
        -- The allowed transitions mirror ActionState::can_transition_to
        -- in crates/crowdrelay-domain/src/action_ledger.rs.
        IF NOT (
            -- PLANNED → AUTHORIZED | CANCELLED | REVOKED
            (current_ledger_state = 'PLANNED' AND new_ledger_state IN ('AUTHORIZED', 'CANCELLED', 'REVOKED'))
            -- AUTHORIZED → QUEUED | CANCELLED | REVOKED
            OR (current_ledger_state = 'AUTHORIZED' AND new_ledger_state IN ('QUEUED', 'CANCELLED', 'REVOKED'))
            -- QUEUED → RUNNING | CANCELLED | FAILED
            OR (current_ledger_state = 'QUEUED' AND new_ledger_state IN ('RUNNING', 'CANCELLED', 'FAILED'))
            -- RUNNING → SUCCEEDED | FAILED | UNKNOWN
            OR (current_ledger_state = 'RUNNING' AND new_ledger_state IN ('SUCCEEDED', 'FAILED', 'UNKNOWN'))
            -- UNKNOWN → RECONCILING | SUCCEEDED | FAILED
            OR (current_ledger_state = 'UNKNOWN' AND new_ledger_state IN ('RECONCILING', 'SUCCEEDED', 'FAILED'))
            -- RECONCILING → SUCCEEDED | FAILED | UNKNOWN
            OR (current_ledger_state = 'RECONCILING' AND new_ledger_state IN ('SUCCEEDED', 'FAILED', 'UNKNOWN'))
            -- SUCCEEDED → FAILED | UNKNOWN
            -- (correction of premature success: actions_execution.rs marks
            -- the action 'succeeded' when dispatching to the executor, before
            -- the external intervention is confirmed. The community executor
            -- may later correct this to 'failed' or 'unknown' when the post
            -- definitively fails or confirmation is lost.)
            OR (current_ledger_state = 'SUCCEEDED' AND new_ledger_state IN ('FAILED', 'UNKNOWN'))
        ) THEN
            RAISE EXCEPTION 'Action ledger: illegal transition from % to % for action %',
                current_ledger_state, new_ledger_state, NEW.id
                USING ERRCODE = 'check_violation';
        END IF;

        -- Apply the transition.
        UPDATE viryaos_action_ledger
        SET state = new_ledger_state,
            state_entered_at = now(),
            updated_at = now(),
            transition_count = transition_count + 1,
            previous_state = current_ledger_state,
            trace_id = COALESCE(viryaos_action_ledger.trace_id, NEW.trace_id)
        WHERE action_id = NEW.id;
    END IF;

    RETURN NEW;
END;
$$;
