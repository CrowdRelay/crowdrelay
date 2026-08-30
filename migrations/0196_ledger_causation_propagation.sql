-- Action Ledger: propagate causation_id from the action to the ledger.
--
-- The action ledger already carries trace_id and decision_id. This
-- migration adds causation_id so the causal chain is visible in the
-- ledger itself, not just in the individual action row.
--
-- The trigger function viryaos_action_ledger_sync() is updated to:
-- 1. Copy causation_id from the action on INSERT (new ledger entry)
-- 2. Backfill causation_id on UPDATE if the ledger row has NULL

ALTER TABLE viryaos_action_ledger
    ADD COLUMN IF NOT EXISTS causation_id uuid;

CREATE INDEX IF NOT EXISTS action_ledger_causation_idx
    ON viryaos_action_ledger (workspace_id, causation_id)
    WHERE causation_id IS NOT NULL;

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
            (action_id, workspace_id, state, trace_id, causation_id, decision_id,
             state_entered_at, transition_count, previous_state)
        VALUES
            (NEW.id, NEW.workspace_id, new_ledger_state, NEW.trace_id,
             NEW.causation_id, NEW.decision_id, now(), 0, NULL);
    ELSIF current_ledger_state = new_ledger_state THEN
        -- No state change — just update trace_id/causation_id if they were NULL.
        UPDATE viryaos_action_ledger
        SET trace_id = COALESCE(viryaos_action_ledger.trace_id, NEW.trace_id),
            causation_id = COALESCE(viryaos_action_ledger.causation_id, NEW.causation_id),
            updated_at = now()
        WHERE action_id = NEW.id;
    ELSE
        -- State transition — enforce the monotonic state machine.
        IF NOT (
            (current_ledger_state = 'PLANNED' AND new_ledger_state IN ('AUTHORIZED', 'CANCELLED', 'REVOKED'))
            OR (current_ledger_state = 'AUTHORIZED' AND new_ledger_state IN ('QUEUED', 'CANCELLED', 'REVOKED'))
            OR (current_ledger_state = 'QUEUED' AND new_ledger_state IN ('RUNNING', 'CANCELLED', 'FAILED'))
            OR (current_ledger_state = 'RUNNING' AND new_ledger_state IN ('SUCCEEDED', 'FAILED', 'UNKNOWN'))
            OR (current_ledger_state = 'UNKNOWN' AND new_ledger_state IN ('RECONCILING', 'SUCCEEDED', 'FAILED'))
            OR (current_ledger_state = 'RECONCILING' AND new_ledger_state IN ('SUCCEEDED', 'FAILED', 'UNKNOWN'))
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
            trace_id = COALESCE(viryaos_action_ledger.trace_id, NEW.trace_id),
            causation_id = COALESCE(viryaos_action_ledger.causation_id, NEW.causation_id)
        WHERE action_id = NEW.id;
    END IF;

    RETURN NEW;
END;
$$;
