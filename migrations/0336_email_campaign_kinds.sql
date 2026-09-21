-- A communication campaign answers for what the send did to the fans it
-- reached: whether delivered recipients bought a ticket, and the
-- unsubscribes the send cost. The unsubscribe kind is the first
-- lower-is-better measurement — harm is a first-class outcome, and a send
-- that only spends fans has to read as one. Clicks stay unmeasured at the
-- send level: the delivery ledger never links a recipient to a click row,
-- so a per-send click count cannot be told apart from the release-level
-- traffic the funnel kinds already observe.
ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;
ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check
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
        'campaign_ticket_conversion_14d','campaign_unsubscribe_7d'
    ));
