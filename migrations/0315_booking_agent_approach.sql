-- The booking-agent approach (Sprint 4.6d, §4h-10): a venue sells the band a
-- room, a promoter a night, and an agent sells the band — so the ask is
-- representation for a season and the refusal closes the door for one.
--
-- Two columns the season needs that 0313 did not yet carry:
--
-- `do_not_contact` is the hardest line in the system. A seasonal refusal is
-- a closed door that opens again; a do-not-contact is a wall, and it has to
-- be a column of its own because filing one as a far-future `refused_until`
-- would be a lie the database is then asked to keep.
--
-- `contact_verified_at` is the route's proof: set when the screened intake
-- promotes the contact (promotion is a human confirmation — a person saw the
-- address and filed it as real) and refreshed by every inbound reply, which
-- is the stronger proof. The gate refuses a row that never had it — a route
-- nobody confirmed is not a route.
ALTER TABLE viryaos_booking_agents
    ADD COLUMN do_not_contact boolean NOT NULL DEFAULT false,
    ADD COLUMN contact_verified_at timestamptz;

-- The agent's own interaction ledger. `viryaos_booking_interactions` cannot
-- hold these rows: its composite foreign key points at booking targets, and
-- an agent is deliberately not one. Same shape, separate table, and the
-- phase vocabulary is the approach's own — an agent gets one letter a
-- season, not an initial/followup cadence.
CREATE TABLE viryaos_booking_agent_interactions (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    agent_id     uuid NOT NULL,
    direction    text NOT NULL CHECK (direction IN ('outbound', 'inbound')),
    phase        text NOT NULL CHECK (phase IN ('approach', 'reply')),
    disposition  text NOT NULL DEFAULT 'none' CHECK (disposition IN (
        'none', 'received', 'positive', 'signed', 'declined', 'do_not_contact'
    )),
    source_key   text NOT NULL CHECK (btrim(source_key) <> '' AND char_length(source_key) <= 200),
    occurred_at  timestamptz NOT NULL,
    metadata     jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    CONSTRAINT viryaos_booking_agent_interactions_agent_fk
        FOREIGN KEY (workspace_id, agent_id)
        REFERENCES viryaos_booking_agents (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, agent_id, source_key)
);
CREATE INDEX viryaos_booking_agent_interactions_agent_time_idx
    ON viryaos_booking_agent_interactions (workspace_id, agent_id, occurred_at DESC, id DESC);

-- The reach ledger needs to name the new recipient.
ALTER TABLE viryaos_reach_events
    DROP CONSTRAINT IF EXISTS viryaos_reach_events_recipient_kind_check;
ALTER TABLE viryaos_reach_events
    ADD CONSTRAINT viryaos_reach_events_recipient_kind_check
    CHECK (recipient_kind IN (
        'fan', 'outreach_target', 'subreddit_audience', 'platform_audience',
        'community', 'telegram_channel', 'discord_channel', 'booking_agent'
    ));

-- A reply to an agent approach is its own measurement: the agent decides on
-- a season's timescale, not a pitch's week. Thirty days is the window the
-- reply is still the approach's answer rather than the season's news.
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
        'incremental_fan_growth_3d','booking_agent_reply_30d'
    ));

-- `booking_agent` is its own autopilot context — a third entity with a
-- season's cadence, not a promoter with a different label. Same widening on
-- all three tables: a decision and its action carry the same context.
ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt','outreach_supply','plays',
        'growth_intelligence','content_strategy','representation',
        'booking_agent'
    ));

ALTER TABLE viryaos_autopilot_decisions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_decisions_context_check;
ALTER TABLE viryaos_autopilot_decisions
    ADD CONSTRAINT viryaos_autopilot_decisions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt','outreach_supply','plays',
        'growth_intelligence','content_strategy','representation',
        'booking_agent'
    ));

ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_context_check;
ALTER TABLE viryaos_autopilot_actions
    ADD CONSTRAINT viryaos_autopilot_actions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt','outreach_supply','plays',
        'growth_intelligence','content_strategy','representation',
        'booking_agent'
    ));

-- Two a day is a backstop, not the cadence — the season is the real bound.
-- require_approval because a real-world introduction always passes a person
-- before it leaves.
INSERT INTO viryaos_autopilot_policies
    (workspace_id, context, max_actions_24h, enabled, autonomy_level,
     minimum_confidence_basis_points)
SELECT id, 'booking_agent', 2, true, 'require_approval', 5000
FROM workspaces
ON CONFLICT (workspace_id, context) DO NOTHING;

CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_autopilot_policies
        (workspace_id, context, max_actions_24h, enabled, autonomy_level,
         minimum_confidence_basis_points)
    VALUES
        -- Drafting, discovery, measurement: unattended.
        (NEW.id, 'growth_intelligence', 30, true, 'bounded_auto', 8000),
        (NEW.id, 'outreach_supply', 2, true, 'bounded_auto', 8000),
        (NEW.id, 'content_supply', 30, true, 'bounded_auto', 8000),
        (NEW.id, 'growth_metrics', 12, true, 'bounded_auto', 8000),
        (NEW.id, 'growth_debt', 10, true, 'bounded_auto', 8000),
        -- Spends money.
        (NEW.id, 'ticket_yield', 10, true, 'require_approval', 8000),
        (NEW.id, 'merchandising', 20, true, 'require_approval', 8000),
        (NEW.id, 'merch_pricing', 10, true, 'require_approval', 8000),
        (NEW.id, 'merch_bundle', 5, true, 'require_approval', 8000),
        (NEW.id, 'promotion_budget', 20, true, 'require_approval', 8000),
        (NEW.id, 'funding', 10, true, 'require_approval', 8000),
        -- Contacts a person.
        (NEW.id, 'fan_lifecycle', 100, true, 'require_approval', 8000),
        (NEW.id, 'outreach', 20, true, 'require_approval', 8000),
        (NEW.id, 'beacon', 12, true, 'require_approval', 8000),
        -- The band approaching an agent or a label — brokered, hidden
        -- address, scarce by the month.
        (NEW.id, 'representation', 4, true, 'require_approval', 5000),
        -- The band approaching a booking agent for representation —
        -- scarcer still: one letter per agent a season, and the evidence
        -- gate binds harder than this cap ever will.
        (NEW.id, 'booking_agent', 2, true, 'require_approval', 5000),
        -- Publishes or commits.
        (NEW.id, 'campaign_lifecycle', 20, true, 'require_approval', 8000),
        (NEW.id, 'release', 30, true, 'require_approval', 8000),
        (NEW.id, 'booking_opportunity', 10, true, 'require_approval', 8000),
        (NEW.id, 'live_opportunity', 15, true, 'require_approval', 8000),
        (NEW.id, 'show_operations', 50, true, 'require_approval', 8000),
        (NEW.id, 'show_growth', 14, true, 'require_approval', 8000),
        (NEW.id, 'experimentation', 10, true, 'require_approval', 8000),
        (NEW.id, 'plays', 40, true, 'require_approval', 8000),
        -- The band decides what it makes. Creative work stays human in
        -- every posture: the engine proposes, the band commits. The 5000
        -- floor matches the evaluator's evidence-derived confidence — a
        -- bare raise scores 6000 — where the inherited 8000 default would
        -- deny most of what the engine ranked.
        (NEW.id, 'content_strategy', 6, true, 'require_approval', 5000)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;
