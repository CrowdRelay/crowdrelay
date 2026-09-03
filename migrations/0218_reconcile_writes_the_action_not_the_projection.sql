-- Reconciliation resolved the projection and left the source untouched.
--
-- `viryaos_action_ledger_reconcile` decided the right answer and then wrote it
-- only to `viryaos_action_ledger`. Nothing updated
-- `viryaos_autopilot_actions.status`, so a reconciled action came out of the
-- function with the ledger reading SUCCEEDED or FAILED while the action itself
-- still read `unknown` — and stayed that way. The two never converged again.
--
-- That inverts the documented authority. `crowdrelay-domain/src/action_ledger.rs`
-- states it plainly: `autopilot_actions.status` is Primary, `action_ledger.state`
-- is a Projection maintained by the `viryaos_autopilot_actions_ledger_sync`
-- trigger, and the projection is one-way. Writing the ledger directly forks it
-- from the row it is supposed to project, and the brain reads the action.
--
-- Two integration tests reported this the whole time. Both assert the reconcile
-- return value — which was always correct — and then assert the action status,
-- which never changed:
--
--   north_star_a_success_lost_unknown_recovery_one_effect
--   north_star_b_unknown_definitive_failure_safe_retry_one_effect
--
-- The fix writes the source and lets the trigger project. The RECONCILING
-- bookkeeping and the still-UNKNOWN branch keep writing the ledger directly,
-- because RECONCILING has no `autopilot_actions.status` counterpart to write.
--
-- No decision logic changes: the strategies, their order, and the returned
-- value are byte-identical to 0214.

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
        -- Write the SOURCE, not the projection. `viryaos_autopilot_actions`
        -- is the primary record of operational state and
        -- `viryaos_action_ledger` is a trigger-maintained projection of it
        -- (`viryaos_autopilot_actions_ledger_sync`, AFTER UPDATE OF status).
        -- Updating the action lets that trigger carry the resolution into the
        -- ledger, which keeps the projection one-way.
        --
        -- `finished_at` is restored because the status CHECK requires it to be
        -- non-NULL for the terminal statuses, and the path that marked the
        -- action `unknown` clears it.
        UPDATE viryaos_autopilot_actions
        SET status = CASE new_state
                         WHEN 'SUCCEEDED' THEN 'succeeded'
                         WHEN 'FAILED' THEN 'failed'
                         WHEN 'CANCELLED' THEN 'cancelled'
                     END,
            finished_at = COALESCE(finished_at, now()),
            updated_at = now()
        WHERE id = target_action_id;
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
