-- Canonical activation/retention reads across identity merges.
--
-- 0391 made retention meaningful, and 0397 deliberately separated engagement
-- from mere session activity. Both still assumed their fan_id argument was the
-- current live row. Append-only/pinned evidence can keep the historical fan id,
-- while conversion readers can hold either side of a later merge. Resolve the
-- exact canonical family; never infer identity from timing.
--
-- Deliberate engagement continues to EXCLUDE fan_sessions. A session/open may
-- support the broader meaningful-retention predicate, but it cannot by itself
-- satisfy the funnel's explicit engagement proof.

CREATE OR REPLACE FUNCTION fan_is_meaningfully_retained(
    p_workspace_id uuid,
    p_fan_id uuid,
    p_acquired_at timestamptz,
    p_observed_at timestamptz
)
RETURNS boolean
LANGUAGE sql
STABLE
PARALLEL SAFE
AS $$
    WITH canonical AS (
        SELECT canonical_fan_id(p_workspace_id, p_fan_id) AS fan_id
    )
    SELECT
        p_workspace_id IS NOT NULL
        AND p_fan_id IS NOT NULL
        AND p_acquired_at IS NOT NULL
        AND p_observed_at IS NOT NULL
        AND p_acquired_at <= p_observed_at - INTERVAL '30 days'
        AND EXISTS (
            SELECT 1
            FROM canonical
            JOIN fans AS fan
              ON fan.workspace_id = p_workspace_id
             AND fan.id = canonical.fan_id
            WHERE fan.status = 'active'
              AND fan.deleted_at IS NULL
              AND fan.merged_into_fan_id IS NULL
              AND COALESCE(
                  (
                      SELECT consent.granted
                      FROM fan_consents AS consent
                      WHERE consent.workspace_id = fan.workspace_id
                        AND consent.fan_id = fan.id
                        AND consent.purpose = 'marketing'
                        AND consent.recorded_at <= p_observed_at
                      ORDER BY consent.recorded_at DESC, consent.id DESC
                      LIMIT 1
                  ),
                  false
              )
              AND fan_has_meaningful_action_between(
                  fan.workspace_id,
                  fan.id,
                  fan.normalized_email,
                  GREATEST(
                      p_acquired_at + INTERVAL '30 days',
                      p_observed_at - INTERVAL '30 days'
                  ),
                  p_observed_at + INTERVAL '1 microsecond'
              )
        );
$$;

COMMENT ON FUNCTION fan_is_meaningfully_retained(uuid, uuid, timestamptz, timestamptz) IS
    'Canonical North-Star retention predicate: historical fan ids resolve to the live identity; mature attributed conversion, active account, current consent, and meaningful post-D30 first-party action are all required.';

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
    WITH canonical AS (
        SELECT canonical_fan_id(p_workspace_id, p_fan_id) AS fan_id
    ), family AS (
        SELECT family.fan_id
        FROM canonical
        CROSS JOIN LATERAL canonical_fan_family(
            p_workspace_id, canonical.fan_id
        ) AS family
    ), emails AS (
        SELECT DISTINCT member.normalized_email AS email
        FROM family
        JOIN fans AS member
          ON member.workspace_id = p_workspace_id
         AND member.id = family.fan_id
        UNION
        SELECT p_email WHERE p_email IS NOT NULL
    )
    SELECT EXISTS (
        SELECT 1
        FROM (
            SELECT orders.paid_at AS occurred_at
            FROM ticket_orders AS orders
            WHERE orders.workspace_id = p_workspace_id
              AND orders.buyer_email IN (SELECT email FROM emails)
              AND orders.status IN ('paid', 'partially_refunded')
              AND orders.paid_at >= p_from
              AND orders.paid_at < p_until

            UNION ALL
            SELECT merch.confirmed_at
            FROM merch_order_facts AS merch
            WHERE merch.workspace_id = p_workspace_id
              AND merch.fan_id IN (SELECT fan_id FROM family)
              AND merch.confirmed_at >= p_from
              AND merch.confirmed_at < p_until

            UNION ALL
            SELECT referral.qualified_at
            FROM referral_attributions AS referral
            CROSS JOIN canonical
            WHERE referral.workspace_id = p_workspace_id
              AND referral.referrer_fan_id IN (SELECT fan_id FROM family)
              AND referral.status = 'qualified'
              AND referral.qualified_at >= p_from
              AND referral.qualified_at < p_until
              AND canonical_qualified_referral_owner_id(
                    p_workspace_id, referral.referred_fan_id
                  ) = canonical.fan_id

            UNION ALL
            SELECT checkin.checked_in_at
            FROM concert_checkins AS checkin
            WHERE checkin.workspace_id = p_workspace_id
              AND checkin.fan_id IN (SELECT fan_id FROM family)
              AND checkin.checked_in_at >= p_from
              AND checkin.checked_in_at < p_until

            UNION ALL
            SELECT pass.redeemed_at
            FROM admission_passes AS pass
            WHERE pass.workspace_id = p_workspace_id
              AND pass.fan_id IN (SELECT fan_id FROM family)
              AND pass.status = 'redeemed'
              AND pass.redeemed_at IS NOT NULL
              AND pass.redeemed_at >= p_from
              AND pass.redeemed_at < p_until

            UNION ALL
            SELECT interest.created_at
            FROM event_interests AS interest
            WHERE interest.workspace_id = p_workspace_id
              AND interest.fan_id IN (SELECT fan_id FROM family)
              AND interest.created_at >= p_from
              AND interest.created_at < p_until

            UNION ALL
            SELECT run.completed_at
            FROM synesthesia_reward_entries AS entry
            JOIN synesthesia_runs AS run
              ON run.workspace_id = entry.workspace_id
             AND run.id = entry.run_id
            WHERE entry.workspace_id = p_workspace_id
              AND entry.fan_id IN (SELECT fan_id FROM family)
              AND NOT run.synthetic
              AND run.completed_at >= p_from
              AND run.completed_at < p_until
        ) AS engagement
        LIMIT 1
    );
$$;

COMMENT ON FUNCTION fan_has_engagement_between(uuid, uuid, text, timestamptz, timestamptz) IS
    'Deliberate canonical fan engagement in a bounded window. Resolves the full identity family and excludes session opens/system contact receipts.';
