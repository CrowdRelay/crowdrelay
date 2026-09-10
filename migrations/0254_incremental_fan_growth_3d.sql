-- A three-day counterfactual, so a strategy belief can move before day 14.
--
-- The strategy posterior learns from `observed_incremental_fans` and from
-- nothing else, and `incremental_fan_growth_14d` was the only measurement kind
-- that produced that column. So no strategy belief could move until fourteen
-- days after a dispatch -- on a brain four days old, with 18 of those
-- measurements pending and the earliest due a week out.
--
-- `incremental_fan_growth_3d` runs the same difference-in-differences
-- arithmetic against a matched three-day pre-period. It is a weaker estimate
-- and is stored as a separate fact rather than as a substitute.

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
        'incremental_fan_growth_3d'
    ));

-- Its own column, never `observed_incremental_fans`.
--
-- The fourteen-day write is `COALESCE(observed_incremental_fans, $3) WHERE
-- observed_incremental_fans IS NULL`, so the first writer wins. A three-day
-- estimate landing there would permanently block the better number arriving
-- eleven days later -- the loop would get faster by becoming permanently
-- worse. Two columns let the learner prefer the fourteen-day estimate wherever
-- it exists and fall back to this one only while it does not.
ALTER TABLE viryaos_growth_evidence
    ADD COLUMN IF NOT EXISTS observed_incremental_fans_3d double precision;

-- The learner asks for rows that have an incremental outcome of either width.
-- Partial, because a row with neither is the common case early on and indexing
-- it would be indexing the absence of the thing.
CREATE INDEX IF NOT EXISTS idx_growth_evidence_incremental_3d
    ON viryaos_growth_evidence (workspace_id, timestamp)
    WHERE observed_incremental_fans_3d IS NOT NULL
      AND observed_incremental_fans IS NULL;
