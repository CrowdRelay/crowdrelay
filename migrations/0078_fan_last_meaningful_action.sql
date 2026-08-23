-- One definition of "this person did something meaningful", in SQL.
--
-- The campaign that matters counts active people, not signups: signed up,
-- consented, and at least one meaningful action inside thirty days. The
-- existing `signal/active_fans` series counts rows whose account status is
-- 'active', which is a statement about the account rather than the person — a
-- fan who signed up two years ago and has done nothing since counts, and that
-- is not the number anybody wants to steer by.
--
-- The rule lives in `crowdrelay_domain::fan_activation`. This function is its
-- set-oriented form, kept as one function rather than copied into every read
-- model so the two cannot drift apart quietly. A contract test asserts the
-- action list here matches `MeaningfulAction::all()`.
--
-- Deliberately strict about what counts. An email open is not an action, an
-- impression is not an action, and a click nobody can tie to a person is not an
-- action. Every branch below is something an identifiable person chose to do
-- and that left a durable first-party row.

CREATE OR REPLACE FUNCTION fan_last_meaningful_action(
    p_workspace_id uuid,
    p_fan_id uuid,
    p_email text
)
RETURNS timestamptz
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    -- GREATEST ignores NULLs and returns NULL only when every branch is NULL,
    -- which is exactly "this person has never done anything".
    SELECT GREATEST(
        -- ticket_purchase: the strongest signal a fan can send.
        (SELECT max(paid_at) FROM ticket_orders
          WHERE workspace_id = p_workspace_id
            AND buyer_email = p_email
            AND status IN ('paid', 'partially_refunded')),
        -- merch_purchase: they bought something.
        (SELECT max(confirmed_at) FROM merch_order_facts
          WHERE workspace_id = p_workspace_id
            AND fan_id = p_fan_id),
        -- qualified_referral: somebody they brought who actually converted.
        (SELECT max(accepted_at) FROM referral_attributions
          WHERE workspace_id = p_workspace_id
            AND referrer_fan_id = p_fan_id),
        -- event_interest: they said they are coming.
        (SELECT max(created_at) FROM event_interests
          WHERE workspace_id = p_workspace_id
            AND fan_id = p_fan_id),
        -- synesthesia_run: a real completed run, never a synthetic one.
        (SELECT max(run.completed_at)
           FROM synesthesia_reward_entries AS entry
           JOIN synesthesia_runs AS run
             ON run.workspace_id = entry.workspace_id
            AND run.id = entry.run_id
          WHERE entry.workspace_id = p_workspace_id
            AND entry.fan_id = p_fan_id
            AND NOT run.synthetic
            AND run.completed_at IS NOT NULL),
        -- signal_session: opening the app. `last_seen_at` rather than
        -- `created_at`, because a session created in March and used yesterday
        -- is somebody who used the app yesterday.
        (SELECT max(last_seen_at) FROM fan_sessions
          WHERE workspace_id = p_workspace_id
            AND fan_id = p_fan_id
            AND revoked_at IS NULL)
    );
$$;

-- The counting query filters on the function's result, so the per-fan lookups
-- it performs need to be cheap. These cover the branches that were not already
-- indexed by fan.
CREATE INDEX IF NOT EXISTS merch_order_facts_fan_confirmed_idx
    ON merch_order_facts (workspace_id, fan_id, confirmed_at DESC);
CREATE INDEX IF NOT EXISTS referral_attributions_referrer_accepted_idx
    ON referral_attributions (workspace_id, referrer_fan_id, accepted_at DESC);
CREATE INDEX IF NOT EXISTS fan_sessions_fan_last_seen_idx
    ON fan_sessions (workspace_id, fan_id, last_seen_at DESC)
    WHERE revoked_at IS NULL;
