-- First-party content fan acquisition measurement.
--
-- A tracked content post can already say how many clicks it earned. This kind
-- closes the funnel by counting the distinct fans whose signup followed that
-- exact tracked-link visit inside the same seven-day window. The Rust enum and
-- parser fail closed on unknown kinds, and the database does the same here.

ALTER TABLE autopilot_measurements
    DROP CONSTRAINT IF EXISTS autopilot_measurements_measurement_kind_check;
ALTER TABLE autopilot_measurements
    ADD CONSTRAINT autopilot_measurements_measurement_kind_check
    CHECK (measurement_kind IN (
        'ticket_revenue_72h','merch_gross_proxy_7d','promotion_roas_7d',
        'booking_reply_7d','outreach_reply_7d','audience_ticket_revenue_72h',
        'show_ticket_revenue_7d','show_growth_surface_clicks_7d',
        'show_growth_attributed_ticket_orders_7d',
        'grassroots_activation_replies_14d','agent_run_fan_growth_14d',
        'agent_run_signal_installs_7d','agent_run_community_engagement_7d',
        'incremental_fan_growth_14d','durable_fan_growth_30d',
        'scanner_discovery_quality_14d','strategist_insight_quality_14d',
        'fan_lifecycle_engagement_7d','agent_run_fan_growth_3d',
        'agent_run_outcome_quality_1h','scanner_discovery_quality_1h',
        'strategist_insight_quality_1h','signal_installs_1d',
        'incremental_fan_growth_3d','booking_agent_reply_30d',
        'show_attendance_rate_14d',
        'release_bound_acquisition_14d','release_link_clicks_14d',
        'release_fan_conversion_14d','release_channel_lift_14d',
        'campaign_ticket_conversion_14d','campaign_unsubscribe_7d',
        'content_link_clicks_7d','content_fan_acquisition_7d',
        'artifact_outcome_7d'
    ));
