-- Content-synergy measurement kinds, and the column that makes one of them
-- observable.
--
-- `content_link_clicks_7d` answers what the tracked link in a social post did
-- — clicks on the `/l/` redirect the post carried, read through the post's own
-- `smart_link_id` rather than the workspace's whole click ledger. The column
-- did not exist: `social_posts.smart_link` stored the draft's raw destination
-- text, and nothing joined a post back to the `smart_links` row the redirect
-- actually ran through. The composite FK follows the workspace-scoped
-- convention — the link and the post belong to the same tenant or the join
-- does not exist.
--
-- `artifact_outcome_7d` answers whether a produced content artifact reached an
-- audience: posts filed against the same content source inside the week after
-- the executor confirmed it. A request that produced something nobody posted
-- reads its real zero; a request that produced nothing never schedules —
-- executor-gated kinds only exist once the receipt says the work landed.
--
-- Telegram and Discord posts get no column: their executors embed links in
-- message text with no `smart_link` field to populate, so there is nothing to
-- resolve at write time. Adding a column no writer fills would be a promise
-- the schema cannot keep.

ALTER TABLE social_posts
    ADD COLUMN smart_link_id uuid;

ALTER TABLE social_posts
    ADD CONSTRAINT social_posts_smart_link_fk
    FOREIGN KEY (workspace_id, smart_link_id)
    REFERENCES smart_links (workspace_id, id)
    ON DELETE RESTRICT;

-- The click-count join is per action: workspace + link + window.
CREATE INDEX social_posts_smart_link_idx
    ON social_posts (workspace_id, smart_link_id)
    WHERE smart_link_id IS NOT NULL;

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
        'campaign_ticket_conversion_14d','campaign_unsubscribe_7d',
        'content_link_clicks_7d','artifact_outcome_7d'
    ));
