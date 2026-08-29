-- Add 'incremental_fan_growth_14d' to the measurement_kind CHECK constraint.
--
-- The IncrementalFanGrowth14d measurement kind was added to the Rust enum
-- (AutopilotMeasurementKind) and the observe/complete handlers, but the DB
-- CHECK constraint was never updated. Attempting to schedule this measurement
-- failed with a CHECK violation, making the observed_incremental_fans
-- evidence path unreachable.
ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;

ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check CHECK (measurement_kind IN (
        'ticket_revenue_72h','merch_gross_proxy_7d','promotion_roas_7d',
        'booking_reply_7d','outreach_reply_7d','audience_ticket_revenue_72h',
        'show_ticket_revenue_7d','show_growth_surface_clicks_7d',
        'show_growth_attributed_ticket_orders_7d','grassroots_activation_replies_14d',
        'agent_run_fan_growth_14d','agent_run_signal_installs_7d',
        'agent_run_community_engagement_7d','incremental_fan_growth_14d'
    ));
