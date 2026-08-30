-- Reconciliation: check outbox delivery status as external truth.
--
-- The SQL fallback function viryaos_action_ledger_reconcile() previously
-- only checked the action's own status (Strategy 1) or community_posts
-- (Strategy 2). For non-community actions that entered UNKNOWN because
-- of ambiguous transport failures (timeout after max attempts), there
-- was no reconciliation strategy — the function returned UNKNOWN.
--
-- This migration adds Strategy 3: check the outbox event's delivery
-- status for non-community actions. The outbox delivery status is the
-- external truth of whether the webhook was eventually delivered:
--   - outbox_events.status = 'delivered' → SUCCEEDED
--   - outbox_events.status = 'dead' + permanent error kind → FAILED
--   - outbox_events.status = 'dead' + ambiguous error kind → UNKNOWN
--   - outbox_events.status = 'pending'/'processing' → UNKNOWN (in flight)
--
-- AUTHORITY:
--   receipt_reconciliation.rs = PRIMARY reconciliation path
--   viryaos_action_ledger_reconcile() = SAFE SQL fallback / manual tool

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
    community_error_message text;
    outbox_status text;
    outbox_error_kind text;
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
    ELSIF action_kind = 'community.engage.request' THEN
        -- Strategy 2: For community engagement, check community_posts.
        SELECT status, error_message INTO community_status, community_error_message
        FROM community_posts
        WHERE action_id = target_action_id
        LIMIT 1;

        IF community_status = 'posted' THEN
            new_state := 'SUCCEEDED';
        ELSIF community_status = 'failed' THEN
            IF community_error_message IS NOT NULL
               AND community_error_message LIKE 'worker crashed during posting%' THEN
                new_state := 'UNKNOWN';
            ELSE
                new_state := 'FAILED';
            END IF;
        ELSE
            new_state := 'UNKNOWN';
        END IF;
    ELSE
        -- Strategy 3: For non-community actions, check the outbox event's
        -- delivery status as external truth. This is the authoritative
        -- reconciliation for actions that entered UNKNOWN because of
        -- ambiguous transport failures.
        SELECT e.status, e.last_error_kind INTO outbox_status, outbox_error_kind
        FROM outbox_events e
        WHERE e.action_id = target_action_id
        ORDER BY e.created_at DESC
        LIMIT 1;

        IF outbox_status = 'delivered' THEN
            -- The webhook was eventually delivered — the side effect happened.
            new_state := 'SUCCEEDED';
        ELSIF outbox_status = 'dead' THEN
            -- Check the error kind to distinguish permanent from ambiguous.
            IF outbox_error_kind IS NOT NULL AND (
                outbox_error_kind LIKE 'http_permanent%'
                OR outbox_error_kind = 'recipient_ineligible'
                OR outbox_error_kind LIKE 'secret_%'
                OR outbox_error_kind LIKE 'endpoint_%'
                OR outbox_error_kind = 'invalid_signing_secret'
                OR outbox_error_kind = 'event_serialization'
            ) THEN
                -- Permanent rejection — definitively failed.
                new_state := 'FAILED';
            ELSE
                -- Ambiguous error (transport_timeout, transport_request,
                -- lease_expired) — stay UNKNOWN. Only external truth from
                -- the provider can resolve this.
                new_state := 'UNKNOWN';
            END IF;
        ELSE
            -- Pending/processing or no outbox event — remain UNKNOWN.
            new_state := 'UNKNOWN';
        END IF;
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
