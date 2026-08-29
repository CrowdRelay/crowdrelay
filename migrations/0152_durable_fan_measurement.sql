-- Add 'durable_fan_growth_30d' to the measurement_kind CHECK constraint.
--
-- The durable fan (Y30) measurement counts fans created in the 14-day
-- post-action window that are still active 30 days after creation (not
-- suppressed, not deleted). This is the North Star metric — fans that
-- stick, not just fans that sign up.
--
-- The brain prefers durable_fans_30d over observed_incremental_fans over
-- observed_fans as the learning target. Until 30 days have passed since
-- the first dispatch, the brain learns from 14-day fan growth as before.

ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;

ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check CHECK (measurement_kind IN (
        'ticket_revenue_72h','merch_gross_proxy_7d','promotion_roas_7d',
        'booking_reply_7d','outreach_reply_7d','audience_ticket_revenue_72h',
        'show_ticket_revenue_7d','show_growth_surface_clicks_7d',
        'show_growth_attributed_ticket_orders_7d','grassroots_activation_replies_14d',
        'agent_run_fan_growth_14d','agent_run_signal_installs_7d',
        'agent_run_community_engagement_7d','incremental_fan_growth_14d',
        'durable_fan_growth_30d'
    ));
