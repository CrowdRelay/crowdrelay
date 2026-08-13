-- VIRYA OS manager layer: local Beacons, human handoffs and operator-owned
-- booking configuration. These tables deliberately reference existing domain
-- facts instead of duplicating them: Autopilot actions remain the approval/job
-- source of truth, events remain the show source of truth, and workspace_members
-- remain the identity source of truth.

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon'
    ));

ALTER TABLE viryaos_autopilot_decisions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_decisions_context_check;
ALTER TABLE viryaos_autopilot_decisions
    ADD CONSTRAINT viryaos_autopilot_decisions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon'
    ));

ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_context_check;
ALTER TABLE viryaos_autopilot_actions
    ADD CONSTRAINT viryaos_autopilot_actions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon'
    ));

INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT id, 'beacon', 12 FROM workspaces
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
        (NEW.id, 'beacon', 12)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;

-- A Beacon is a local amplifier around a market/show, not a fan record and not
-- an arbitrary CRM contact. Discovery adapters may propose rows, but automatic
-- outreach is restricted to verified destinations that explicitly remain active.
CREATE TABLE viryaos_beacons (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    city_id uuid REFERENCES cities(id) ON DELETE SET NULL,
    beacon_kind text NOT NULL CHECK (beacon_kind IN (
        'radio','local_press','television','reviewer','creator',
        'photographer','promoter','patron','community'
    )),
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 240),
    contact_email text CHECK (
        contact_email IS NULL OR (
            char_length(contact_email) <= 320
            AND contact_email ~* '^[^[:space:]@]+@[^[:space:]@]+[.][^[:space:]@]+$'
        )
    ),
    destination_url text CHECK (destination_url IS NULL OR char_length(destination_url) <= 2048),
    source_url text CHECK (source_url IS NULL OR char_length(source_url) <= 2048),
    active boolean NOT NULL DEFAULT true,
    verified boolean NOT NULL DEFAULT false,
    accepts_outreach boolean NOT NULL DEFAULT false,
    do_not_contact boolean NOT NULL DEFAULT false,
    relationship_score integer NOT NULL DEFAULT 50 CHECK (relationship_score BETWEEN 0 AND 100),
    relevance_basis_points integer NOT NULL DEFAULT 5000 CHECK (relevance_basis_points BETWEEN 0 AND 10000),
    confidence_basis_points integer NOT NULL DEFAULT 5000 CHECK (confidence_basis_points BETWEEN 0 AND 10000),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE NULLS NOT DISTINCT (workspace_id, beacon_kind, city_id, contact_email)
);
CREATE TRIGGER viryaos_beacons_set_updated_at
BEFORE UPDATE ON viryaos_beacons
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_beacons_local_idx
    ON viryaos_beacons (workspace_id, city_id, beacon_kind, relevance_basis_points DESC, id)
    WHERE active AND verified AND accepts_outreach AND NOT do_not_contact;

-- Per-show relationship state keeps the global Beacon record clean and prevents
-- a successful patronage/interview from being re-pitched by the next campaign tick.
CREATE TABLE viryaos_beacon_campaigns (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    beacon_id uuid NOT NULL,
    event_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'candidate' CHECK (status IN (
        'candidate','contacted','interested','partner','declined','suppressed','closed'
    )),
    last_phase text CHECK (last_phase IS NULL OR last_phase IN (
        'initial','collaboration_follow_up','local_push','post_show_thanks'
    )),
    last_reply_disposition text NOT NULL DEFAULT 'none' CHECK (last_reply_disposition IN (
        'none','received','interested','partner','declined','do_not_contact'
    )),
    last_outreach_at timestamptz,
    followup_count integer NOT NULL DEFAULT 0 CHECK (followup_count BETWEEN 0 AND 8),
    attributable_reach integer CHECK (attributable_reach IS NULL OR attributable_reach >= 0),
    notes text CHECK (notes IS NULL OR char_length(notes) <= 2000),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, beacon_id, event_id),
    CONSTRAINT viryaos_beacon_campaigns_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_beacon_campaigns_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id) ON DELETE CASCADE
);
CREATE TRIGGER viryaos_beacon_campaigns_set_updated_at
BEFORE UPDATE ON viryaos_beacon_campaigns
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_beacon_campaigns_due_idx
    ON viryaos_beacon_campaigns (workspace_id, event_id, status, last_outreach_at, beacon_id)
    WHERE status IN ('candidate','contacted');

-- Routing metadata is intentionally separate from member identity/email. Emails
-- enter workspace_members from deploy secrets/config sync; Git only contains
-- non-secret member keys/skills.
CREATE TABLE viryaos_team_profiles (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    member_id uuid NOT NULL,
    member_key text NOT NULL CHECK (member_key ~ '^[a-z0-9_-]{2,48}$'),
    active boolean NOT NULL DEFAULT true,
    skills text[] NOT NULL DEFAULT ARRAY[]::text[],
    capacity_basis_points integer NOT NULL DEFAULT 10000 CHECK (capacity_basis_points BETWEEN 0 AND 10000),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, member_id),
    UNIQUE (workspace_id, member_key),
    CONSTRAINT viryaos_team_profiles_member_fk
        FOREIGN KEY (workspace_id, member_id)
        REFERENCES workspace_members (workspace_id, id) ON DELETE CASCADE
);
CREATE TRIGGER viryaos_team_profiles_set_updated_at
BEFORE UPDATE ON viryaos_team_profiles
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

-- This is a handoff index, not a second task system. The action/checklist/domain
-- record referenced by source_* remains authoritative for what actually happens.
CREATE TABLE viryaos_team_assignments (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id uuid,
    source_kind text NOT NULL CHECK (source_kind IN (
        'autopilot_action','show_task','opportunity','beacon'
    )),
    source_id uuid NOT NULL,
    assignee_member_id uuid NOT NULL,
    required_skill text NOT NULL CHECK (btrim(required_skill) <> '' AND char_length(required_skill) <= 64),
    status text NOT NULL DEFAULT 'open' CHECK (status IN ('open','done','cancelled')),
    due_at timestamptz,
    assigned_at timestamptz NOT NULL DEFAULT now(),
    last_reminded_at timestamptz,
    next_reminder_at timestamptz,
    reminder_count integer NOT NULL DEFAULT 0 CHECK (reminder_count BETWEEN 0 AND 12),
    completed_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE NULLS NOT DISTINCT (workspace_id, action_id),
    CONSTRAINT viryaos_team_assignments_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_team_assignments_member_fk
        FOREIGN KEY (workspace_id, assignee_member_id)
        REFERENCES workspace_members (workspace_id, id) ON DELETE RESTRICT,
    CHECK ((status = 'done') = (completed_at IS NOT NULL))
);
CREATE TRIGGER viryaos_team_assignments_set_updated_at
BEFORE UPDATE ON viryaos_team_assignments
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_team_assignments_owner_due_idx
    ON viryaos_team_assignments (workspace_id, assignee_member_id, due_at, assigned_at, id)
    WHERE status = 'open';
CREATE INDEX viryaos_team_assignments_reminder_idx
    ON viryaos_team_assignments (next_reminder_at, workspace_id, id)
    WHERE status = 'open' AND next_reminder_at IS NOT NULL;

-- Operator-editable, versioned manager policy cache. Google Sheets/n8n is an
-- ingestion adapter only; this row is the last-valid durable value used at run time.
CREATE TABLE viryaos_manager_config (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    config_key text NOT NULL CHECK (config_key ~ '^[a-z0-9_.-]{2,80}$'),
    value jsonb NOT NULL CHECK (jsonb_typeof(value) = 'object'),
    source text NOT NULL DEFAULT 'database' CHECK (source IN ('database','google_sheets','operator')),
    source_revision text CHECK (source_revision IS NULL OR char_length(source_revision) <= 200),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    synced_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, config_key)
);
CREATE TRIGGER viryaos_manager_config_set_updated_at
BEFORE UPDATE ON viryaos_manager_config
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

INSERT INTO viryaos_manager_config (workspace_id, config_key, value)
SELECT id, 'booking_policy', jsonb_build_object(
    'annual_target', 15,
    'annual_stretch', 20,
    'stretch_minimum_score_basis_points', 9000,
    'prefer_weekend_one_shots', true,
    'priority_markets', jsonb_build_array('PL','DE-EAST','CZ','SK'),
    'far_shot_minimum_score_basis_points', 9000
) FROM workspaces
ON CONFLICT (workspace_id, config_key) DO NOTHING;

-- Travel/date facts are typed columns because booking policy must not depend on
-- opaque metadata JSON. Existing rows stay valid and are simply treated as unknown.
ALTER TABLE viryaos_team_opportunities
    ADD COLUMN event_starts_at timestamptz,
    ADD COLUMN country_code text CHECK (country_code IS NULL OR country_code ~ '^[A-Z]{2}$'),
    ADD COLUMN travel_band text CHECK (travel_band IS NULL OR travel_band IN (
        'poland','east_germany','czechia_slovakia','far_shot'
    ));
CREATE INDEX viryaos_team_opportunities_live_calendar_idx
    ON viryaos_team_opportunities (workspace_id, event_starts_at, fit_basis_points DESC, id)
    WHERE opportunity_kind IN ('festival','showcase','support_slot')
      AND status NOT IN ('lost','dismissed');
