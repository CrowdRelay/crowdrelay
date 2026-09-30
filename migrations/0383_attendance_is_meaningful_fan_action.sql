-- Observed attendance is a canonical meaningful fan action.
--
-- Product semantics already treat attendance as conversion, but the canonical
-- activation/retention functions omitted it. That made a consented fan who
-- physically attended a show look inactive unless another action happened to
-- occur as well.
--
-- Attendance is evidence only when CrowdRelay owns the fact:
--   * a concert QR check-in, or
--   * a redeemed admission pass.
-- Ticket purchase alone remains a separate action and is never used as an
-- attendance proxy.
--
-- The KPI still requires an open account and current marketing consent. This
-- migration changes what counts as meaningful behaviour, not who may be
-- contacted.

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
    SELECT GREATEST(
        -- ticket_purchase: the strongest commercial signal a fan can send.
        (SELECT max(paid_at)
           FROM ticket_orders
          WHERE workspace_id = p_workspace_id
            AND buyer_email = p_email
            AND status IN ('paid', 'partially_refunded')),
        -- merch_purchase: they bought something from the band.
        (SELECT max(confirmed_at)
           FROM merch_order_facts
          WHERE workspace_id = p_workspace_id
            AND fan_id = p_fan_id),
        -- qualified_referral: somebody they brought actually converted.
        (SELECT max(qualified_at)
           FROM referral_attributions
          WHERE workspace_id = p_workspace_id
            AND referrer_fan_id = p_fan_id
            AND status = 'qualified'
            AND qualified_at IS NOT NULL),
        -- attendance: the person was observably in the room.
        (SELECT max(checked_in_at)
           FROM concert_checkins
          WHERE workspace_id = p_workspace_id
            AND fan_id = p_fan_id),
        (SELECT max(redeemed_at)
           FROM admission_passes
          WHERE workspace_id = p_workspace_id
            AND fan_id = p_fan_id
            AND status = 'redeemed'
            AND redeemed_at IS NOT NULL),
        -- event_interest: they said they are coming.
        (SELECT max(created_at)
           FROM event_interests
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
        -- signal_session: opening Signal. A revoked session is not activity.
        (SELECT max(last_seen_at)
           FROM fan_sessions
          WHERE workspace_id = p_workspace_id
            AND fan_id = p_fan_id
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

            UNION ALL
            -- signal_session:
            SELECT last_seen_at
              FROM fan_sessions
             WHERE workspace_id = p_workspace_id
               AND fan_id = p_fan_id
               AND revoked_at IS NULL
               AND last_seen_at >= p_from
               AND last_seen_at < p_until
        ) AS meaningful
        LIMIT 1
    );
$$;

-- Re-state the canonical KPI with the updated action function. Consent and
-- account-state semantics are intentionally unchanged.
CREATE OR REPLACE VIEW fan_activation_kpi AS
SELECT
    fan.workspace_id AS workspace_id,
    count(*) FILTER (
        WHERE fan.created_at >= now() - INTERVAL '30 days'
    ) AS signups_30d,
    count(*) FILTER (
        WHERE fan.status = 'active'
          AND consent.granted
          AND fan.created_at >= now() - INTERVAL '30 days'
          AND activity.last_action_at IS NOT NULL
          AND activity.last_action_at >= fan.created_at
          AND activity.last_action_at <= now()
          AND activity.last_action_at <= fan.created_at + INTERVAL '30 days'
    ) AS activated_30d,
    count(*) FILTER (
        WHERE activity.last_action_at >= now() - INTERVAL '30 days'
          AND activity.last_action_at <= now()
    ) AS active_30d,
    count(*) FILTER (
        WHERE fan.status = 'active'
          AND consent.granted
    ) AS reachable_consented,
    count(*) FILTER (
        WHERE fan_has_meaningful_action_between(
                  fan.workspace_id,
                  fan.id,
                  fan.normalized_email,
                  now() - INTERVAL '30 days',
                  now()
              )
          AND fan_has_meaningful_action_between(
                  fan.workspace_id,
                  fan.id,
                  fan.normalized_email,
                  now() - INTERVAL '60 days',
                  now() - INTERVAL '30 days'
              )
    ) AS retained_30d
FROM fans AS fan
LEFT JOIN LATERAL (
    SELECT granted
      FROM fan_consents AS consent
     WHERE consent.workspace_id = fan.workspace_id
       AND consent.fan_id = fan.id
       AND consent.purpose = 'marketing'
     ORDER BY consent.recorded_at DESC, consent.id DESC
     LIMIT 1
) AS consent ON true
LEFT JOIN LATERAL (
    SELECT fan_last_meaningful_action(
        fan.workspace_id,
        fan.id,
        fan.normalized_email
    ) AS last_action_at
) AS activity ON true
GROUP BY fan.workspace_id;

-- Keep the denormalised cache aligned for cheap sorting/read paths.
UPDATE fans AS fan
SET last_activity_at = fan_last_meaningful_action(
    fan.workspace_id,
    fan.id,
    fan.normalized_email
)
WHERE fan.last_activity_at IS DISTINCT FROM fan_last_meaningful_action(
    fan.workspace_id,
    fan.id,
    fan.normalized_email
);

-- The system-owned series changed definition. Preserve cycle-run audit history,
-- but never splice pre-attendance and post-attendance points into one series.
DELETE FROM growth_metric_points AS point
USING growth_metric_series AS series
WHERE point.workspace_id = series.workspace_id
  AND point.series_id = series.id
  AND series.platform = 'signal'
  AND series.metric_key = 'activated_fans_30d';
