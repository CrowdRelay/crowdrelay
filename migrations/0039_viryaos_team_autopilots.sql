-- VIRYA OS team autopilots: release orchestration, profitable live opportunities,
-- funding preparation, calendar intents and media-patronage outreach.
--
-- Keep the model intentionally small: the existing lifecycle/outreach/commerce
-- bounded contexts stay authoritative. This migration only adds the missing
-- first-class facts that let them do real work for the band.

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding'
    ));

ALTER TABLE viryaos_autopilot_decisions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_decisions_context_check;
ALTER TABLE viryaos_autopilot_decisions
    ADD CONSTRAINT viryaos_autopilot_decisions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding'
    ));

ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_context_check;
ALTER TABLE viryaos_autopilot_actions
    ADD CONSTRAINT viryaos_autopilot_actions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding'
    ));

INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT workspace.id, context.name, context.max_actions_24h
FROM workspaces AS workspace
CROSS JOIN (VALUES
    ('release', 30),
    ('live_opportunity', 15),
    ('funding', 10)
) AS context(name, max_actions_24h)
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
        (NEW.id, 'funding', 10)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;

-- Media patronage uses the proven relationship-aware Outreach bounded context.
ALTER TABLE viryaos_outreach_targets
    DROP CONSTRAINT IF EXISTS viryaos_outreach_targets_target_kind_check;
ALTER TABLE viryaos_outreach_targets
    ADD CONSTRAINT viryaos_outreach_targets_target_kind_check CHECK (target_kind IN (
        'playlist','radio','press','creator','support_slot','endorsement','media_patronage'
    ));

CREATE TABLE viryaos_release_plans (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    source_key text NOT NULL CHECK (btrim(source_key) <> '' AND char_length(source_key) <= 200),
    title text NOT NULL CHECK (btrim(title) <> '' AND char_length(title) <= 240),
    release_at timestamptz NOT NULL,
    listen_url text CHECK (listen_url IS NULL OR char_length(listen_url) <= 2048),
    active boolean NOT NULL DEFAULT true,
    assets_ready boolean NOT NULL DEFAULT false,
    communication_enabled boolean NOT NULL DEFAULT true,
    press_enabled boolean NOT NULL DEFAULT true,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, source_key)
);
CREATE TRIGGER viryaos_release_plans_set_updated_at
BEFORE UPDATE ON viryaos_release_plans
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_release_plans_due_idx
    ON viryaos_release_plans (workspace_id, release_at, id)
    WHERE active;

CREATE TABLE viryaos_release_milestones (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    release_id uuid NOT NULL,
    milestone text NOT NULL CHECK (milestone IN (
        'seed_calendar','announcement','start_press','fan_warmup',
        'countdown','release_day','sustain','wrap'
    )),
    action_id uuid NOT NULL,
    completed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, release_id, milestone),
    UNIQUE (workspace_id, action_id),
    CONSTRAINT viryaos_release_milestones_release_fk
        FOREIGN KEY (workspace_id, release_id)
        REFERENCES viryaos_release_plans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT viryaos_release_milestones_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE RESTRICT
);

-- Durable provider-neutral calendar requests. n8n/Calendar is an executor only;
-- CrowdRelay owns the reason, timing and idempotency.
CREATE TABLE viryaos_calendar_requests (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    source_kind text NOT NULL CHECK (source_kind IN ('release','opportunity','funding','show')),
    source_id uuid NOT NULL,
    calendar_key text NOT NULL CHECK (btrim(calendar_key) <> '' AND char_length(calendar_key) <= 240),
    title text NOT NULL CHECK (btrim(title) <> '' AND char_length(title) <= 240),
    starts_at timestamptz NOT NULL,
    action_id uuid NOT NULL,
    outbox_event_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, calendar_key),
    UNIQUE (workspace_id, outbox_event_id),
    CONSTRAINT viryaos_calendar_requests_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT viryaos_calendar_requests_outbox_fk
        FOREIGN KEY (workspace_id, outbox_event_id)
        REFERENCES outbox_events (workspace_id, id)
        ON DELETE RESTRICT
);

-- Candidates discovered by deterministic/provider adapters. The adapter reports
-- facts; Rust decides whether an application is worth preparing or sending.
CREATE TABLE viryaos_team_opportunities (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    opportunity_kind text NOT NULL CHECK (opportunity_kind IN (
        'festival','showcase','review_contest','support_slot','funding'
    )),
    source text NOT NULL CHECK (btrim(source) <> '' AND char_length(source) <= 80),
    external_key text NOT NULL CHECK (btrim(external_key) <> '' AND char_length(external_key) <= 240),
    title text NOT NULL CHECK (btrim(title) <> '' AND char_length(title) <= 240),
    organization text NOT NULL CHECK (btrim(organization) <> '' AND char_length(organization) <= 240),
    destination_url text CHECK (destination_url IS NULL OR char_length(destination_url) <= 2048),
    contact_email text CHECK (
        contact_email IS NULL OR (
            char_length(contact_email) <= 320
            AND contact_email ~* '^[^[:space:]@]+@[^[:space:]@]+[.][^[:space:]@]+$'
        )
    ),
    verified_destination boolean NOT NULL DEFAULT false,
    fit_basis_points integer NOT NULL CHECK (fit_basis_points BETWEEN 0 AND 10000),
    reputation_basis_points integer NOT NULL DEFAULT 5000 CHECK (reputation_basis_points BETWEEN 0 AND 10000),
    confidence_basis_points integer NOT NULL CHECK (confidence_basis_points BETWEEN 0 AND 10000),
    currency text NOT NULL DEFAULT 'PLN' CHECK (currency ~ '^[A-Z]{3}$'),
    expected_fee_minor bigint NOT NULL DEFAULT 0 CHECK (expected_fee_minor >= 0),
    estimated_cost_minor bigint NOT NULL DEFAULT 0 CHECK (estimated_cost_minor >= 0),
    application_fee_minor bigint NOT NULL DEFAULT 0 CHECK (application_fee_minor >= 0),
    requires_contract boolean NOT NULL DEFAULT false,
    exclusive boolean NOT NULL DEFAULT false,
    eligible boolean NOT NULL DEFAULT true,
    funding_amount_minor bigint NOT NULL DEFAULT 0 CHECK (funding_amount_minor >= 0),
    own_contribution_minor bigint NOT NULL DEFAULT 0 CHECK (own_contribution_minor >= 0),
    deadline timestamptz,
    package_status text NOT NULL DEFAULT 'none' CHECK (package_status IN ('none','requested','ready')),
    status text NOT NULL DEFAULT 'new' CHECK (status IN (
        'new','prepared','awaiting_approval','submission_requested','submitted','replied','won','lost','dismissed'
    )),
    last_action_at timestamptz,
    followup_count integer NOT NULL DEFAULT 0 CHECK (followup_count BETWEEN 0 AND 5),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata)='object'),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, source, external_key)
);
CREATE TRIGGER viryaos_team_opportunities_set_updated_at
BEFORE UPDATE ON viryaos_team_opportunities
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_team_opportunities_due_idx
    ON viryaos_team_opportunities (workspace_id, opportunity_kind, deadline, fit_basis_points DESC, id)
    WHERE status IN ('new','prepared','awaiting_approval') AND eligible;
