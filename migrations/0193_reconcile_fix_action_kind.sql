-- Fix action_kind mismatch in viryaos_action_ledger_reconcile.
--
-- The function (introduced in 0186, rewritten crash-aware in 0192) used
-- `action_kind = 'community.engage'` but the actual action_kind value
-- produced by the autopilot brain is `'community.engage.request'`
-- (see AutopilotActionPayload::RequestCommunityEngagement in model.rs).
-- The mismatch meant the SQL fallback never matched community engagement
-- actions, so Strategy 2 (community_posts lookup) was dead code.
--
-- This migration recreates the function with the correct action_kind
-- string and the crash-aware logic from 0192.
--
-- CRITICAL SEMANTIC RULE (unchanged from 0192):
--   community_posts.status = 'failed' only implies failed treatment when
--   the failure reason establishes that the external side effect did not
--   occur. A crash-marked failure means confirmation was lost, NOT that
--   the intervention failed.
--
-- The crash marker is the error_message prefix 'worker crashed during posting',
-- centralized as CRASH_POSTING_ERROR_PREFIX in community_executor.rs. If that
-- prefix changes, this function MUST be updated together.
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
    -- Note: 'cancelled' is NOT handled here — if the action was cancelled,
    -- the trigger on viryaos_autopilot_actions already transitioned the
    -- ledger to CANCELLED. Reconciliation only resolves UNKNOWN states.
    IF action_status = 'succeeded' THEN
        new_state := 'SUCCEEDED';
    ELSIF action_status = 'failed' THEN
        new_state := 'FAILED';
    ELSIF action_kind = 'community.engage.request' THEN
        -- Strategy 2: For community engagement, check community_posts.
        -- Fetch both status and error_message to distinguish crash-marked
        -- failures (confirmation lost) from definitive executor failures.
        SELECT status, error_message INTO community_status, community_error_message
        FROM community_posts
        WHERE action_id = target_action_id
        LIMIT 1;

        IF community_status = 'posted' THEN
            -- The post exists on Reddit — the intervention is confirmed.
            new_state := 'SUCCEEDED';
        ELSIF community_status = 'failed' THEN
            -- Distinguish crash-marked failures from definitive failures.
            -- A crash-marked failure means CrowdRelay lost confirmation
            -- after the external call may already have succeeded. Only a
            -- human checking Reddit can tell. Stay UNKNOWN.
            --
            -- The crash marker prefix MUST stay in sync with
            -- CRASH_POSTING_ERROR_PREFIX in community_executor.rs.
            IF community_error_message IS NOT NULL
               AND community_error_message LIKE 'worker crashed during posting%' THEN
                new_state := 'UNKNOWN';
            ELSE
                -- Definitive executor failure — the post definitively
                -- never went out (pre-Reddit rejection, no agents service,
                -- subreddit cooldown, etc.).
                new_state := 'FAILED';
            END IF;
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
