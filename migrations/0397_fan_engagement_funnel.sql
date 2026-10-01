CREATE OR REPLACE FUNCTION fan_has_engagement_between(
    p_workspace_id uuid,
    p_fan_id uuid,
    p_email text,
    p_from timestamptz,
    p_until timestamptz
)
RETURNS boolean
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    SELECT EXISTS (
        SELECT 1
        FROM (
            -- ticket_purchase:
            SELECT paid_at AS occurred_at
              FROM ticket_orders
             WHERE workspace_id = p_workspace_id
               AND buyer_email = p_email
               AND status IN ('paid', 'partially_refunded')
               AND paid_at >= p_from
               AND paid_at < p_until

            UNION ALL
            -- merch_purchase:
            SELECT confirmed_at
              FROM merch_order_facts
             WHERE workspace_id = p_workspace_id
               AND fan_id = p_fan_id
               AND confirmed_at >= p_from
               AND confirmed_at < p_until

            UNION ALL
            -- qualified_referral:
            SELECT qualified_at
              FROM referral_attributions
             WHERE workspace_id = p_workspace_id
               AND referrer_fan_id = p_fan_id
               AND status = 'qualified'
               AND qualified_at >= p_from
               AND qualified_at < p_until

            UNION ALL
            -- attendance:
            SELECT checked_in_at
              FROM concert_checkins
             WHERE workspace_id = p_workspace_id
               AND fan_id = p_fan_id
               AND checked_in_at >= p_from
               AND checked_in_at < p_until

            UNION ALL
            SELECT redeemed_at
              FROM admission_passes
             WHERE workspace_id = p_workspace_id
               AND fan_id = p_fan_id
               AND status = 'redeemed'
               AND redeemed_at IS NOT NULL
               AND redeemed_at >= p_from
               AND redeemed_at < p_until

            UNION ALL
            -- event_interest:
            SELECT created_at
              FROM event_interests
             WHERE workspace_id = p_workspace_id
               AND fan_id = p_fan_id
               AND created_at >= p_from
               AND created_at < p_until

            UNION ALL
            -- synesthesia_run:
            SELECT run.completed_at
              FROM synesthesia_reward_entries AS entry
              JOIN synesthesia_runs AS run
                ON run.workspace_id = entry.workspace_id
               AND run.id = entry.run_id
             WHERE entry.workspace_id = p_workspace_id
               AND entry.fan_id = p_fan_id
               AND NOT run.synthetic
               AND run.completed_at >= p_from
               AND run.completed_at < p_until

            
        ) AS meaningful
        LIMIT 1
    );
$$;
