-- Action Ledger trigger: automatically populate the ledger when
-- autopilot actions are created or their status changes.
--
-- The trigger maps the autopilot action status to the ledger state:
--   awaiting_approval → AUTHORIZED
--   queued            → QUEUED
--   in_progress        → RUNNING
--   succeeded          → SUCCEEDED
--   failed             → FAILED
--   cancelled          → CANCELLED
--   parked             → PLANNED
--
-- The trigger enforces the monotonic transition rules: illegal backwards
-- transitions are rejected (the trigger raises an exception).

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
        WHEN 'in_progress' THEN 'RUNNING'
        WHEN 'succeeded' THEN 'SUCCEEDED'
        WHEN 'failed' THEN 'FAILED'
        WHEN 'cancelled' THEN 'CANCELLED'
        WHEN 'parked' THEN 'PLANNED'
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
        -- State transition — check monotonicity.
        -- Terminal states reject all transitions.
        IF current_ledger_state IN ('SUCCEEDED', 'FAILED', 'CANCELLED', 'REVOKED') THEN
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

-- Drop the old trigger if it exists (idempotent).
DROP TRIGGER IF EXISTS viryaos_autopilot_actions_ledger_sync ON viryaos_autopilot_actions;

-- Create the trigger to fire after INSERT or UPDATE of status.
CREATE TRIGGER viryaos_autopilot_actions_ledger_sync
AFTER INSERT OR UPDATE OF status ON viryaos_autopilot_actions
FOR EACH ROW
EXECUTE FUNCTION viryaos_action_ledger_sync();
