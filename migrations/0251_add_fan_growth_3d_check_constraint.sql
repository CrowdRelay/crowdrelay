-- Fix: migration 0248 added the agent_run_fan_growth_3d measurement kind
-- in Rust but assumed "measurement_kind is a text column" with no CHECK
-- constraint. The table DOES have a CHECK constraint
-- (viryaos_autopilot_measurements_measurement_kind_check) that whitelist
-- the allowed kinds. Without this fix, any attempt to insert a
-- measurement with kind='agent_run_fan_growth_3d' fails with a CHECK
-- violation, which surfaces as an "unexpected" repository error and
-- marks the action as failed.

ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check;

ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check
    CHECK (measurement_kind = ANY (ARRAY[
        'ticket_revenue_72h'::text,
        'merch_gross_proxy_7d'::text,
        'promotion_roas_7d'::text,
        'booking_reply_7d'::text,
        'outreach_reply_7d'::text,
        'audience_ticket_revenue_72h'::text,
        'show_ticket_revenue_7d'::text,
        'show_growth_surface_clicks_7d'::text,
        'show_growth_attributed_ticket_orders_7d'::text,
        'grassroots_activation_replies_14d'::text,
        'agent_run_fan_growth_14d'::text,
        'agent_run_fan_growth_3d'::text,
        'agent_run_signal_installs_7d'::text,
        'agent_run_community_engagement_7d'::text,
        'incremental_fan_growth_14d'::text,
        'durable_fan_growth_30d'::text,
        'scanner_discovery_quality_14d'::text,
        'strategist_insight_quality_14d'::text,
        'fan_lifecycle_engagement_7d'::text
    ]));

-- Fix: the action ledger state machine (viryaos_action_ledger_sync trigger)
-- does not allow RUNNING → QUEUED transitions. The fail_action retry logic
-- needs this: when a retryable error occurs and attempt_count < 5, the
-- action is set back to 'queued' for retry. Without this fix, the retry
-- fails with "illegal transition from RUNNING to QUEUED", the action
-- remains stuck in 'processing', and the stale-recovery sweep eventually
-- reaps it as 'failed' after 2 hours — wasting the attempt budget and
-- delaying the learning loop.

CREATE OR REPLACE FUNCTION public.viryaos_action_ledger_sync()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
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
        -- RUNNING → QUEUED is allowed for retryable failures (fail_action
        -- sets status back to 'queued' when attempt_count < 5).
        IF NOT (
            (current_ledger_state = 'PLANNED' AND new_ledger_state IN ('AUTHORIZED', 'CANCELLED', 'REVOKED'))
            OR (current_ledger_state = 'AUTHORIZED' AND new_ledger_state IN ('QUEUED', 'CANCELLED', 'REVOKED'))
            OR (current_ledger_state = 'QUEUED' AND new_ledger_state IN ('RUNNING', 'CANCELLED', 'FAILED'))
            OR (current_ledger_state = 'RUNNING' AND new_ledger_state IN ('SUCCEEDED', 'FAILED', 'UNKNOWN', 'QUEUED'))
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
$function$;
