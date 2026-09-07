-- Add 'scanner_discovery_quality_14d' and 'strategist_insight_quality_14d'
-- to the measurement_kind CHECK constraint.
--
-- These proximal-outcome measurement kinds were added to the Rust enum
-- (AutopilotMeasurementKind::ScannerDiscoveryQuality14d and
-- StrategistInsightQuality14d) but the CHECK constraint was never updated,
-- causing every scanner/strategist dispatch to fail with a CHECK violation
-- when the measurement row was inserted.

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
    'strategist_insight_quality_14d'::text
]));
