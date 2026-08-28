-- Add measurement kinds for agent dispatch effect measurement.
--
-- The brain now measures whether its LLM worker dispatches actually produce
-- fan growth, Signal installs, and community engagement. These three kinds
-- close the learning loop: the brain dispatches a worker, measures the
-- effect, and adjusts the worker's standing (cooldown / tier / retirement)
-- based on the outcome.
--
-- - agent_run_fan_growth_14d: new fans acquired in the 14-day window after
--   a worker dispatch. The brain's primary success metric.
-- - agent_run_signal_installs_7d: new Signal push endpoints installed in the
--   7-day window after a dispatch. Measures direct conversion.
-- - agent_run_community_engagement_7d: aggregated community post engagement
--   (upvotes, comments, score) in the 7-day window after a community-engager
--   dispatch. Measures reach and engagement quality.

ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;

ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check CHECK (measurement_kind IN (
        'ticket_revenue_72h','merch_gross_proxy_7d','promotion_roas_7d',
        'booking_reply_7d','outreach_reply_7d','audience_ticket_revenue_72h',
        'show_ticket_revenue_7d','show_growth_surface_clicks_7d',
        'show_growth_attributed_ticket_orders_7d','grassroots_activation_replies_14d',
        'agent_run_fan_growth_14d','agent_run_signal_installs_7d',
        'agent_run_community_engagement_7d'
    ));
