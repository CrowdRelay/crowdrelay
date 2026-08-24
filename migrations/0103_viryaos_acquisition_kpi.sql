-- The campaign's primary KPI, derived rather than stored.
--
-- Channel identity already exists on smart links (migration 0079: which
-- source, community and creative brought each fan in), and the acquisition
-- read model already attributes signups and activations per channel through
-- click events. What nothing answered was the brief's headline number for the
-- whole workspace: how many people are actually HERE, deduplicated and
-- recent.
--
-- This view is deliberately a VIEW. A stored KPI goes stale silently the day
-- somebody stops refreshing it; a view recomputes from the facts on every
-- read:
--
--   signups_30d        arrivals this month (the funnel's top)
--   activated_30d      signed up AND consented AND did something meaningful
--                      within 30 days of signing up — the brief's definition,
--                      verbatim
--   active_30d         did something meaningful in the last 30 days,
--                      however they arrived — retention, not acquisition
--   reachable_consented everyone contactable right now, whatever their
--                      signup date

CREATE VIEW viryaos_fan_activation_kpi AS
SELECT
    fan.workspace_id AS workspace_id,
    count(*) FILTER (WHERE fan.created_at >= now() - INTERVAL '30 days')
        AS signups_30d,
    count(*) FILTER (
        WHERE consent.granted
          AND fan.created_at >= now() - INTERVAL '30 days'
          AND fan.last_activity_at IS NOT NULL
          AND fan.last_activity_at <= fan.created_at + INTERVAL '30 days'
    ) AS activated_30d,
    count(*) FILTER (WHERE fan.last_activity_at >= now() - INTERVAL '30 days')
        AS active_30d,
    count(*) FILTER (
        WHERE fan.status = 'active' AND consent.granted
    ) AS reachable_consented
FROM fans AS fan
LEFT JOIN LATERAL (
    SELECT granted
    FROM fan_consents AS c
    WHERE c.workspace_id = fan.workspace_id
      AND c.fan_id = fan.id
      AND c.purpose = 'marketing'
    ORDER BY c.recorded_at DESC, c.id DESC
    LIMIT 1
) AS consent ON true
GROUP BY fan.workspace_id;
