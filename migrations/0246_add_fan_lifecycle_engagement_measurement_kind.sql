-- Add 'fan_lifecycle_engagement_7d' to the measurement_kind CHECK constraint.
--
-- This per-fan outcome measurement closes the learning loop for lifecycle
-- messaging (welcome, re-engagement, referral invite). The observer counts
-- ticket orders, Signal push endpoint creations, and referral redemptions by
-- the specific fan who received the message in the 7-day post-action window.
-- The baseline is 0 — lifecycle messages target new or dormant fans who
-- haven't engaged yet.

ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;

ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check CHECK (measurement_kind = ANY (ARRAY[
    'ticket_revenue_72h'::text,
    'merch_gross_proxy_7d'::text,
    'promotion_roas_7d'::text,
    'booking_reply_7d'::text,
    'outreach_reply_7d'::text,
    'audience_ticket_revenue_72h'::text,
    'show_ticket_revenue_7d'::text,
    'show_growth_surface_clicks_7d'::text,
    'show_growth_attributed_ticket_orders_7d'::text,
    'grassroots_activation_replies_14d'::text,
    'agent_run_fan_growth_14d'::text,
    'agent_run_signal_installs_7d'::text,
    'agent_run_community_engagement_7d'::text,
    'incremental_fan_growth_14d'::text,
    'durable_fan_growth_30d'::text,
    'scanner_discovery_quality_14d'::text,
    'strategist_insight_quality_14d'::text,
    'fan_lifecycle_engagement_7d'::text
]));
