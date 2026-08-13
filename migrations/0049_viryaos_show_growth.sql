-- Attendance Growth / Demand Loop.
--
-- This adds a deterministic event-growth context without introducing a second
-- campaign/task system. Existing events, audience segments, communications,
-- Beacons, ticketing and merch inventory remain the durable facts.

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth'
    ));

ALTER TABLE viryaos_autopilot_decisions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_decisions_context_check;
ALTER TABLE viryaos_autopilot_decisions
    ADD CONSTRAINT viryaos_autopilot_decisions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth'
    ));

ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_context_check;
ALTER TABLE viryaos_autopilot_actions
    ADD CONSTRAINT viryaos_autopilot_actions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth'
    ));

INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT id, 'show_growth', 14 FROM workspaces
ON CONFLICT (workspace_id, context) DO NOTHING;

CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
    VALUES
        (NEW.id, 'ticket_yield', 10),
        (NEW.id, 'fan_lifecycle', 100),
        (NEW.id, 'campaign_lifecycle', 20),
        (NEW.id, 'merchandising', 20),
        (NEW.id, 'merch_pricing', 10),
        (NEW.id, 'merch_bundle', 5),
        (NEW.id, 'booking_opportunity', 10),
        (NEW.id, 'outreach', 20),
        (NEW.id, 'content_supply', 30),
        (NEW.id, 'promotion_budget', 20),
        (NEW.id, 'experimentation', 10),
        (NEW.id, 'show_operations', 50),
        (NEW.id, 'release', 30),
        (NEW.id, 'live_opportunity', 15),
        (NEW.id, 'funding', 10),
        (NEW.id, 'beacon', 12),
        (NEW.id, 'show_growth', 14)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;

-- A venue or another scene actor can be a Beacon when it lends a local
-- audience/distribution channel to a specific show. This keeps cross-promo in
-- the same verified/suppressed relationship boundary as media outreach.
ALTER TABLE viryaos_beacons
    DROP CONSTRAINT IF EXISTS viryaos_beacons_beacon_kind_check;
ALTER TABLE viryaos_beacons
    ADD CONSTRAINT viryaos_beacons_beacon_kind_check CHECK (beacon_kind IN (
        'radio','local_press','television','reviewer','creator',
        'photographer','promoter','venue','scene_partner','patron','community'
    ));

ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;
ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check CHECK (measurement_kind IN (
        'ticket_revenue_72h','merch_gross_proxy_7d','promotion_roas_7d',
        'booking_reply_7d','outreach_reply_7d','audience_ticket_revenue_72h',
        'show_ticket_revenue_7d'
    ));
