-- Close the named-Beacon learning loop.
--
-- A relationship action is only useful to the North Star if CrowdRelay can
-- tell what that exact action produced. Posts already recover action identity
-- through their publication ledgers; Beacon email has no post row. Give smart
-- links an optional action owner so the existing click -> visitor -> signup
-- spine can attribute a fan to the named relationship without a second
-- analytics system.
--
-- Replies need their own immutable occurrence timestamp too. `updated_at`
-- also moves on later outbound follow-ups, so it cannot honestly answer
-- whether a reply landed inside this action's window. Existing rows are not
-- backfilled from `updated_at`: an unknown historical reply time stays
-- unknown rather than becoming fabricated evidence.

ALTER TABLE smart_links
    ADD COLUMN IF NOT EXISTS action_id uuid;

ALTER TABLE smart_links
    DROP CONSTRAINT IF EXISTS smart_links_action_fk;
ALTER TABLE smart_links
    ADD CONSTRAINT smart_links_action_fk
    FOREIGN KEY (workspace_id, action_id)
    REFERENCES autopilot_actions (workspace_id, id)
    ON DELETE RESTRICT;

CREATE INDEX IF NOT EXISTS smart_links_action_idx
    ON smart_links (workspace_id, action_id)
    WHERE action_id IS NOT NULL;

ALTER TABLE beacon_campaigns
    ADD COLUMN IF NOT EXISTS last_reply_at timestamptz;

CREATE INDEX IF NOT EXISTS beacon_campaigns_reply_idx
    ON beacon_campaigns (workspace_id, beacon_id, event_id, last_reply_at DESC)
    WHERE last_reply_at IS NOT NULL;

-- Fail closed on measurement vocabulary. Keep the historical constraint name:
-- the contract test and older installations know it, while the table itself
-- now uses the unprefixed runtime name.
ALTER TABLE autopilot_measurements
    DROP CONSTRAINT IF EXISTS autopilot_measurements_measurement_kind_check;
ALTER TABLE autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;
ALTER TABLE autopilot_measurements
    ADD CONSTRAINT autopilot_measurements_measurement_kind_check
    CHECK (measurement_kind IN (
        'ticket_revenue_72h','merch_gross_proxy_7d','promotion_roas_7d',
        'booking_reply_7d','outreach_reply_7d','audience_ticket_revenue_72h',
        'show_ticket_revenue_7d','show_growth_surface_clicks_7d',
        'show_growth_attributed_ticket_orders_7d',
        'grassroots_activation_replies_14d',
        'beacon_outreach_reply_14d',
        'beacon_outreach_unique_visitors_14d',
        'agent_run_fan_growth_14d','agent_run_signal_installs_7d',
        'agent_run_community_engagement_7d','incremental_fan_growth_14d',
        'durable_fan_growth_30d','scanner_discovery_quality_14d',
        'strategist_insight_quality_14d','fan_lifecycle_engagement_7d',
        'agent_run_fan_growth_3d','agent_run_outcome_quality_1h',
        'scanner_discovery_quality_1h','strategist_insight_quality_1h',
        'signal_installs_1d','incremental_fan_growth_3d',
        'booking_agent_reply_30d','show_attendance_rate_14d',
        'release_bound_acquisition_14d','release_link_clicks_14d',
        'release_fan_conversion_14d','release_channel_lift_14d',
        'campaign_ticket_conversion_14d','campaign_unsubscribe_7d',
        'content_link_clicks_7d','content_fan_acquisition_7d',
        'artifact_outcome_7d'
    ));
