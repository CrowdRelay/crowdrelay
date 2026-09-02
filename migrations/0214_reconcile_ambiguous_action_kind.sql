-- Reconciliation could never run: a variable shadowed the column it read.
--
--     action_kind text;                      -- PL/pgSQL variable
--     SELECT status, action_kind INTO action_status, action_kind
--
-- Postgres cannot tell whether `action_kind` in the select list means the
-- column or the variable, so every call raised
--
--     42702: column reference "action_kind" is ambiguous
--
-- The function is the resolver for actions whose outcome is uncertain. It was
-- introduced in 0186, rewritten in 0192, and corrected in 0193 and 0195 —
-- none of which could have worked, because the first statement past the state
-- guard always threw.
--
-- Nothing has hit it in production yet: reconciliation only runs for ledger
-- rows in UNKNOWN, and no action has entered UNKNOWN so far. That is luck, not
-- safety. UNKNOWN is exactly where an action lands when a provider times out
-- or a confirmation is lost, and the row could never have left it.
--
-- Five integration tests have been reporting this the whole time. They were
-- failing alongside twenty-one others and had stopped being read.
--
-- The fix is a rename: `action_kind` becomes `v_action_kind`, and the source
-- table is aliased so both sides of the INTO are unambiguous. No logic changes.

CREATE OR REPLACE FUNCTION viryaos_action_ledger_reconcile(target_action_id uuid)
RETURNS text
LANGUAGE plpgsql
AS $reconcile$

DECLARE
    current_state text;
    action_status text;
    v_action_kind text;
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
    SELECT a.status, a.action_kind INTO action_status, v_action_kind
    FROM viryaos_autopilot_actions AS a
    WHERE a.id = target_action_id;

    -- Strategy 1: Check if the action status has been updated.
    IF action_status = 'succeeded' THEN
        new_state := 'SUCCEEDED';
    ELSIF action_status = 'failed' THEN
        new_state := 'FAILED';
    ELSIF v_action_kind = 'community.engage.request' THEN
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
$reconcile$;
