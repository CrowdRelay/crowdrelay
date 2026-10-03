-- Canonical fan activity must survive identity merges.
--
-- fan identity merge intentionally leaves append-only/pinned history (notably
-- referral_attributions) on the historical fan row. The meaningful-action
-- functions previously read only p_fan_id, so the same human could appear to
-- lose a real referral/activity after canonicalization. Resolve the exact
-- identity family first; never use timing proximity as identity evidence.
--
-- Self-referrals that collapse to the same canonical identity are excluded.
-- The family walk is index-backed by fans(workspace_id, merged_into_fan_id)
-- and bounded to one person's identity tree.

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
    WITH RECURSIVE lineage(id, merged_into_fan_id) AS (
        SELECT fan.id, fan.merged_into_fan_id
          FROM fans fan
         WHERE fan.workspace_id = p_workspace_id
           AND fan.id = p_fan_id
        UNION ALL
        SELECT parent.id, parent.merged_into_fan_id
          FROM lineage child
          JOIN fans parent
            ON parent.workspace_id = p_workspace_id
           AND parent.id = child.merged_into_fan_id
    ), root(id) AS (
        SELECT id
          FROM lineage
         WHERE merged_into_fan_id IS NULL
         LIMIT 1
    ), family(member_id) AS (
        SELECT id FROM root
        UNION ALL
        SELECT child.id
          FROM family
          JOIN fans child
            ON child.workspace_id = p_workspace_id
           AND child.merged_into_fan_id = family.member_id
    ), emails(email) AS (
        SELECT member.normalized_email
          FROM family
          JOIN fans member
            ON member.workspace_id = p_workspace_id
           AND member.id = family.member_id
        UNION
        SELECT p_email WHERE p_email IS NOT NULL
    )
    SELECT GREATEST(
        (SELECT max(paid_at)
           FROM ticket_orders
          WHERE workspace_id = p_workspace_id
            AND buyer_email IN (SELECT email FROM emails)
            AND status IN ('paid', 'partially_refunded')),
        (SELECT max(confirmed_at)
           FROM merch_order_facts
          WHERE workspace_id = p_workspace_id
            AND fan_id IN (SELECT member_id FROM family)),
        (SELECT max(referral.qualified_at)
           FROM referral_attributions referral
          WHERE referral.workspace_id = p_workspace_id
            AND referral.referrer_fan_id IN (SELECT member_id FROM family)
            AND referral.status = 'qualified'
            AND referral.qualified_at IS NOT NULL
            AND NOT EXISTS (
                SELECT 1 FROM family same_person
                 WHERE same_person.member_id = referral.referred_fan_id
            )),
        (SELECT max(checked_in_at)
           FROM concert_checkins
          WHERE workspace_id = p_workspace_id
            AND fan_id IN (SELECT member_id FROM family)),
        (SELECT max(redeemed_at)
           FROM admission_passes
          WHERE workspace_id = p_workspace_id
            AND fan_id IN (SELECT member_id FROM family)
            AND status = 'redeemed'
            AND redeemed_at IS NOT NULL),
        (SELECT max(created_at)
           FROM event_interests
          WHERE workspace_id = p_workspace_id
            AND fan_id IN (SELECT member_id FROM family)),
        (SELECT max(run.completed_at)
           FROM synesthesia_reward_entries entry
           JOIN synesthesia_runs run
             ON run.workspace_id = entry.workspace_id
            AND run.id = entry.run_id
          WHERE entry.workspace_id = p_workspace_id
            AND entry.fan_id IN (SELECT member_id FROM family)
            AND NOT run.synthetic
            AND run.completed_at IS NOT NULL),
        (SELECT max(last_seen_at)
           FROM fan_sessions
          WHERE workspace_id = p_workspace_id
            AND fan_id IN (SELECT member_id FROM family)
            AND revoked_at IS NULL)
    );
$$;

CREATE OR REPLACE FUNCTION fan_has_meaningful_action_between(
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
    WITH RECURSIVE lineage(id, merged_into_fan_id) AS (
        SELECT fan.id, fan.merged_into_fan_id
          FROM fans fan
         WHERE fan.workspace_id = p_workspace_id
           AND fan.id = p_fan_id
        UNION ALL
        SELECT parent.id, parent.merged_into_fan_id
          FROM lineage child
          JOIN fans parent
            ON parent.workspace_id = p_workspace_id
           AND parent.id = child.merged_into_fan_id
    ), root(id) AS (
        SELECT id
          FROM lineage
         WHERE merged_into_fan_id IS NULL
         LIMIT 1
    ), family(member_id) AS (
        SELECT id FROM root
        UNION ALL
        SELECT child.id
          FROM family
          JOIN fans child
            ON child.workspace_id = p_workspace_id
           AND child.merged_into_fan_id = family.member_id
    ), emails(email) AS (
        SELECT member.normalized_email
          FROM family
          JOIN fans member
            ON member.workspace_id = p_workspace_id
           AND member.id = family.member_id
        UNION
        SELECT p_email WHERE p_email IS NOT NULL
    )
    SELECT EXISTS (
        SELECT 1
          FROM (
            SELECT paid_at AS occurred_at
              FROM ticket_orders
             WHERE workspace_id = p_workspace_id
               AND buyer_email IN (SELECT email FROM emails)
               AND status IN ('paid', 'partially_refunded')
               AND paid_at >= p_from AND paid_at < p_until

            UNION ALL
            SELECT confirmed_at
              FROM merch_order_facts
             WHERE workspace_id = p_workspace_id
               AND fan_id IN (SELECT member_id FROM family)
               AND confirmed_at >= p_from AND confirmed_at < p_until

            UNION ALL
            SELECT referral.qualified_at
              FROM referral_attributions referral
             WHERE referral.workspace_id = p_workspace_id
               AND referral.referrer_fan_id IN (SELECT member_id FROM family)
               AND referral.status = 'qualified'
               AND referral.qualified_at >= p_from
               AND referral.qualified_at < p_until
               AND NOT EXISTS (
                   SELECT 1 FROM family same_person
                    WHERE same_person.member_id = referral.referred_fan_id
               )

            UNION ALL
            SELECT checked_in_at
              FROM concert_checkins
             WHERE workspace_id = p_workspace_id
               AND fan_id IN (SELECT member_id FROM family)
               AND checked_in_at >= p_from AND checked_in_at < p_until

            UNION ALL
            SELECT redeemed_at
              FROM admission_passes
             WHERE workspace_id = p_workspace_id
               AND fan_id IN (SELECT member_id FROM family)
               AND status = 'redeemed'
               AND redeemed_at IS NOT NULL
               AND redeemed_at >= p_from AND redeemed_at < p_until

            UNION ALL
            SELECT created_at
              FROM event_interests
             WHERE workspace_id = p_workspace_id
               AND fan_id IN (SELECT member_id FROM family)
               AND created_at >= p_from AND created_at < p_until

            UNION ALL
            SELECT run.completed_at
              FROM synesthesia_reward_entries entry
              JOIN synesthesia_runs run
                ON run.workspace_id = entry.workspace_id
               AND run.id = entry.run_id
             WHERE entry.workspace_id = p_workspace_id
               AND entry.fan_id IN (SELECT member_id FROM family)
               AND NOT run.synthetic
               AND run.completed_at >= p_from AND run.completed_at < p_until

            UNION ALL
            SELECT last_seen_at
              FROM fan_sessions
             WHERE workspace_id = p_workspace_id
               AND fan_id IN (SELECT member_id FROM family)
               AND revoked_at IS NULL
               AND last_seen_at >= p_from AND last_seen_at < p_until
          ) meaningful
         LIMIT 1
    );
$$;
