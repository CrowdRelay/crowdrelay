-- ViryaOS autonomous operations plane, phase 2.
--
-- Additive bounded contexts for lifecycle campaigns, relationship outreach,
-- content supply, merch bundles, experiments and show operations. External
-- adapters continue to report facts or execute signed intents; deterministic
-- Rust domains remain the only decision authority.

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield', 'fan_lifecycle', 'campaign_lifecycle',
        'merchandising', 'merch_pricing', 'merch_bundle',
        'booking_opportunity', 'outreach', 'content_supply',
        'promotion_budget', 'experimentation', 'show_operations'
    ));

ALTER TABLE viryaos_autopilot_decisions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_decisions_context_check;
ALTER TABLE viryaos_autopilot_decisions
    ADD CONSTRAINT viryaos_autopilot_decisions_context_check CHECK (context IN (
        'ticket_yield', 'fan_lifecycle', 'campaign_lifecycle',
        'merchandising', 'merch_pricing', 'merch_bundle',
        'booking_opportunity', 'outreach', 'content_supply',
        'promotion_budget', 'experimentation', 'show_operations'
    ));

ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_context_check;
ALTER TABLE viryaos_autopilot_actions
    ADD CONSTRAINT viryaos_autopilot_actions_context_check CHECK (context IN (
        'ticket_yield', 'fan_lifecycle', 'campaign_lifecycle',
        'merchandising', 'merch_pricing', 'merch_bundle',
        'booking_opportunity', 'outreach', 'content_supply',
        'promotion_budget', 'experimentation', 'show_operations'
    ));

INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT workspace.id, context.name, context.max_actions_24h
FROM workspaces AS workspace
CROSS JOIN (VALUES
    ('campaign_lifecycle', 20),
    ('merch_bundle', 5),
    ('outreach', 20),
    ('content_supply', 30),
    ('experimentation', 10),
    ('show_operations', 50)
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
        (NEW.id, 'show_operations', 50)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;

-- Optional operator-verified capacity lets the Booking domain prefer a venue
-- that fits first-party demand without making external discovery authoritative.
ALTER TABLE viryaos_booking_targets
    ADD COLUMN IF NOT EXISTS capacity integer
    CHECK (capacity IS NULL OR capacity BETWEEN 1 AND 100000);
ALTER TABLE viryaos_booking_target_history
    ADD COLUMN IF NOT EXISTS capacity integer
    CHECK (capacity IS NULL OR capacity BETWEEN 1 AND 100000);

-- Booking communication history is append-only. It prevents autonomous
-- follow-ups after a human reply and gives the outcome loop explicit evidence.
CREATE TABLE viryaos_booking_interactions (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_id uuid NOT NULL,
    direction text NOT NULL CHECK (direction IN ('outbound', 'inbound')),
    phase text NOT NULL CHECK (phase IN ('initial', 'followup', 'reply')),
    disposition text NOT NULL DEFAULT 'none' CHECK (disposition IN (
        'none', 'received', 'positive', 'declined', 'booked', 'do_not_contact'
    )),
    source_key text NOT NULL CHECK (btrim(source_key) <> '' AND char_length(source_key) <= 200),
    occurred_at timestamptz NOT NULL,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    CONSTRAINT viryaos_booking_interactions_target_fk
        FOREIGN KEY (workspace_id, target_id)
        REFERENCES viryaos_booking_targets (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, target_id, source_key)
);
CREATE INDEX viryaos_booking_interactions_target_time_idx
    ON viryaos_booking_interactions (workspace_id, target_id, occurred_at DESC, id DESC);

-- Financial authority is explicit and currency-scoped. Promotion controllers
-- may lower spend without a guardrail, but any autonomous increase must fit
-- both caps using the latest provider-reported month-to-date spend.
CREATE TABLE viryaos_promotion_budget_guardrails (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    currency text NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    maximum_total_daily_budget_minor bigint NOT NULL
        CHECK (maximum_total_daily_budget_minor > 0),
    maximum_monthly_spend_minor bigint NOT NULL
        CHECK (maximum_monthly_spend_minor > 0),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, currency)
);

CREATE TRIGGER viryaos_promotion_budget_guardrails_set_updated_at
BEFORE UPDATE ON viryaos_promotion_budget_guardrails
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_promotion_budget_guardrail_history (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    currency text NOT NULL,
    version bigint NOT NULL CHECK (version > 0),
    maximum_total_daily_budget_minor bigint NOT NULL,
    maximum_monthly_spend_minor bigint NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, currency, version),
    CONSTRAINT viryaos_promotion_budget_guardrail_history_fk
        FOREIGN KEY (workspace_id, currency)
        REFERENCES viryaos_promotion_budget_guardrails (workspace_id, currency)
        ON DELETE CASCADE
);

-- Short-lived reservations close the race between a provider snapshot and an
-- emitted budget-increase request. They are deliberately conservative: a
-- successful request reserves its positive delta for 24h, until fresh provider
-- state can naturally supersede it.
CREATE TABLE viryaos_promotion_budget_reservations (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id uuid NOT NULL,
    campaign_id uuid NOT NULL,
    currency text NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    daily_delta_minor bigint NOT NULL CHECK (daily_delta_minor > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, action_id),
    CONSTRAINT viryaos_promotion_budget_reservation_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_promotion_budget_reservation_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES viryaos_promotion_campaign_states (workspace_id, id) ON DELETE CASCADE,
    CHECK (expires_at > created_at)
);
CREATE INDEX viryaos_promotion_budget_reservations_active_idx
    ON viryaos_promotion_budget_reservations (workspace_id, currency, expires_at, action_id);

-- Operator-owned relationship graph for non-booking outreach. A provider or
-- crawler may propose an opportunity, but only verified targets are eligible.
CREATE TABLE viryaos_outreach_targets (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_kind text NOT NULL CHECK (target_kind IN (
        'playlist', 'radio', 'press', 'creator', 'support_slot', 'endorsement'
    )),
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 200),
    contact_email text NOT NULL CHECK (
        char_length(contact_email) <= 320
        AND contact_email ~* '^[^[:space:]@]+@[^[:space:]@]+\.[^[:space:]@]+$'
    ),
    active boolean NOT NULL DEFAULT true,
    verified boolean NOT NULL DEFAULT false,
    accepts_outreach boolean NOT NULL DEFAULT true,
    priority integer NOT NULL DEFAULT 50 CHECK (priority BETWEEN 0 AND 100),
    relationship_score integer NOT NULL DEFAULT 50 CHECK (relationship_score BETWEEN 0 AND 100),
    do_not_contact boolean NOT NULL DEFAULT false,
    last_outreach_at timestamptz,
    last_reply_at timestamptz,
    last_reply_disposition text NOT NULL DEFAULT 'none' CHECK (last_reply_disposition IN (
        'none', 'received', 'positive', 'declined', 'do_not_contact'
    )),
    followup_count integer NOT NULL DEFAULT 0 CHECK (followup_count BETWEEN 0 AND 10),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, contact_email)
);
CREATE TRIGGER viryaos_outreach_targets_set_updated_at
BEFORE UPDATE ON viryaos_outreach_targets
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_outreach_targets_eligible_idx
    ON viryaos_outreach_targets (workspace_id, target_kind, priority DESC, relationship_score DESC, id)
    WHERE active AND verified AND accepts_outreach AND NOT do_not_contact;

CREATE TABLE viryaos_outreach_target_history (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_id uuid NOT NULL,
    version bigint NOT NULL CHECK (version > 0),
    snapshot jsonb NOT NULL CHECK (jsonb_typeof(snapshot) = 'object'),
    recorded_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_outreach_target_history_target_fk
        FOREIGN KEY (workspace_id, target_id)
        REFERENCES viryaos_outreach_targets (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, target_id, version)
);

CREATE TABLE viryaos_outreach_opportunities (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_id uuid NOT NULL,
    source text NOT NULL CHECK (btrim(source) <> '' AND char_length(source) <= 64),
    subject_kind text NOT NULL CHECK (subject_kind IN ('release', 'event', 'catalogue', 'band')),
    subject_key text NOT NULL CHECK (btrim(subject_key) <> '' AND char_length(subject_key) <= 200),
    template_key text NOT NULL CHECK (btrim(template_key) <> '' AND char_length(template_key) <= 160),
    relevance_basis_points integer NOT NULL CHECK (relevance_basis_points BETWEEN 0 AND 10000),
    confidence_basis_points integer NOT NULL CHECK (confidence_basis_points BETWEEN 0 AND 10000),
    active boolean NOT NULL DEFAULT true,
    observed_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_outreach_opportunities_target_fk
        FOREIGN KEY (workspace_id, target_id)
        REFERENCES viryaos_outreach_targets (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, source, target_id, subject_kind, subject_key),
    CHECK (expires_at > observed_at)
);
CREATE TRIGGER viryaos_outreach_opportunities_set_updated_at
BEFORE UPDATE ON viryaos_outreach_opportunities
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_outreach_opportunities_due_idx
    ON viryaos_outreach_opportunities (workspace_id, expires_at, relevance_basis_points DESC, id)
    WHERE active;

CREATE TABLE viryaos_outreach_interactions (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_id uuid NOT NULL,
    opportunity_id uuid,
    direction text NOT NULL CHECK (direction IN ('outbound', 'inbound')),
    phase text NOT NULL CHECK (phase IN ('initial', 'followup', 'reply')),
    disposition text NOT NULL DEFAULT 'none' CHECK (disposition IN (
        'none', 'received', 'positive', 'declined', 'do_not_contact'
    )),
    source_key text NOT NULL CHECK (btrim(source_key) <> '' AND char_length(source_key) <= 200),
    occurred_at timestamptz NOT NULL,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    CONSTRAINT viryaos_outreach_interactions_target_fk
        FOREIGN KEY (workspace_id, target_id)
        REFERENCES viryaos_outreach_targets (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT viryaos_outreach_interactions_opportunity_fk
        FOREIGN KEY (workspace_id, opportunity_id)
        REFERENCES viryaos_outreach_opportunities (workspace_id, id)
        ON DELETE SET NULL (opportunity_id),
    UNIQUE (workspace_id, target_id, source_key)
);
CREATE INDEX viryaos_outreach_interactions_target_time_idx
    ON viryaos_outreach_interactions (workspace_id, target_id, occurred_at DESC, id DESC);

-- Trusted source facts for deterministic content production. No generated copy
-- is persisted here; adapters receive an artifact request plus approved template.
CREATE TABLE viryaos_content_sources (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    source_kind text NOT NULL CHECK (source_kind IN ('event', 'release', 'show_completed')),
    source_key text NOT NULL CHECK (btrim(source_key) <> '' AND char_length(source_key) <= 200),
    title text NOT NULL CHECK (btrim(title) <> '' AND char_length(title) <= 240),
    occurred_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, source_kind, source_key),
    CHECK (expires_at > occurred_at)
);
CREATE TRIGGER viryaos_content_sources_set_updated_at
BEFORE UPDATE ON viryaos_content_sources
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_content_sources_active_idx
    ON viryaos_content_sources (workspace_id, expires_at, occurred_at DESC, id)
    WHERE active;


-- Project trusted event facts into the content bounded context. This trigger is
-- deliberately a fact projection only: it never chooses copy, channel or an
-- executable action. Those decisions remain in deterministic Rust.
CREATE OR REPLACE FUNCTION viryaos_project_event_content_sources()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.status IN ('published','completed') THEN
        INSERT INTO viryaos_content_sources(
            id,workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
        ) VALUES(
            NEW.id,NEW.workspace_id,'event','event:' || NEW.id::text,NEW.title,now(),
            GREATEST(NEW.starts_at + INTERVAL '14 days', now() + INTERVAL '7 days'),
            jsonb_build_object('event_id',NEW.id,'slug',NEW.slug,'venue',NEW.venue,'starts_at',NEW.starts_at,'city_id',NEW.city_id),true
        )
        ON CONFLICT(workspace_id,source_kind,source_key) DO UPDATE SET
            title=EXCLUDED.title,expires_at=EXCLUDED.expires_at,metadata=EXCLUDED.metadata,
            active=true,version=viryaos_content_sources.version+1;
    ELSE
        UPDATE viryaos_content_sources
        SET active=false,version=version+1
        WHERE workspace_id=NEW.workspace_id AND source_kind='event' AND source_key='event:' || NEW.id::text AND active;
    END IF;

    IF NEW.status = 'completed' THEN
        IF TG_OP = 'INSERT'
           OR (TG_OP = 'UPDATE' AND OLD.status IS DISTINCT FROM 'completed') THEN
            INSERT INTO viryaos_content_sources(
                workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
            ) VALUES(
                NEW.workspace_id,'show_completed','show_completed:' || NEW.id::text,NEW.title,now(),now()+INTERVAL '45 days',
                jsonb_build_object('event_id',NEW.id,'slug',NEW.slug,'venue',NEW.venue,'starts_at',NEW.starts_at,'city_id',NEW.city_id),true
            )
            ON CONFLICT(workspace_id,source_kind,source_key) DO UPDATE SET
                title=EXCLUDED.title,metadata=EXCLUDED.metadata,expires_at=EXCLUDED.expires_at,
                active=true,version=viryaos_content_sources.version+1;
        END IF;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER viryaos_events_project_content_sources
AFTER INSERT OR UPDATE OF status,title,starts_at,venue,city_id ON events
FOR EACH ROW EXECUTE FUNCTION viryaos_project_event_content_sources();

-- Backfill currently active shows during the migration so the content supply
-- chain becomes useful immediately instead of waiting for the next edit.
INSERT INTO viryaos_content_sources(
    id,workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata,active
)
SELECT event.id,event.workspace_id,'event','event:' || event.id::text,event.title,now(),
       GREATEST(event.starts_at + INTERVAL '14 days',now()+INTERVAL '7 days'),
       jsonb_build_object('event_id',event.id,'slug',event.slug,'venue',event.venue,'starts_at',event.starts_at,'city_id',event.city_id),true
FROM events event
WHERE event.status IN ('published','completed')
ON CONFLICT(workspace_id,source_kind,source_key) DO NOTHING;

CREATE TABLE viryaos_content_source_history (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    source_id uuid NOT NULL,
    version bigint NOT NULL CHECK (version > 0),
    snapshot jsonb NOT NULL CHECK (jsonb_typeof(snapshot) = 'object'),
    recorded_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_content_source_history_source_fk
        FOREIGN KEY (workspace_id, source_id)
        REFERENCES viryaos_content_sources (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, source_id, version)
);

-- Aggregate experiment engine. It intentionally stores only counters/value,
-- not fan-level behavioral profiles. Variants are reallocated by deterministic
-- Rust after minimum-sample and bounded-step guards are satisfied.
CREATE TABLE viryaos_experiments (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    metric_kind text NOT NULL CHECK (metric_kind IN ('conversion', 'revenue_per_exposure')),
    status text NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'running', 'paused', 'completed')),
    winner_variant_id uuid,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug)
);
CREATE TRIGGER viryaos_experiments_set_updated_at
BEFORE UPDATE ON viryaos_experiments
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_experiment_variants (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    experiment_id uuid NOT NULL,
    variant_key text NOT NULL CHECK (btrim(variant_key) <> '' AND char_length(variant_key) <= 96),
    allocation_basis_points integer NOT NULL CHECK (allocation_basis_points BETWEEN 0 AND 10000),
    exposures bigint NOT NULL DEFAULT 0 CHECK (exposures >= 0),
    conversions bigint NOT NULL DEFAULT 0 CHECK (conversions >= 0 AND conversions <= exposures),
    value_minor bigint NOT NULL DEFAULT 0 CHECK (value_minor >= 0),
    active boolean NOT NULL DEFAULT true,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, experiment_id, variant_key),
    CONSTRAINT viryaos_experiment_variants_experiment_fk
        FOREIGN KEY (workspace_id, experiment_id)
        REFERENCES viryaos_experiments (workspace_id, id)
        ON DELETE CASCADE
);
CREATE TRIGGER viryaos_experiment_variants_set_updated_at
BEFORE UPDATE ON viryaos_experiment_variants
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
ALTER TABLE viryaos_experiments
    ADD CONSTRAINT viryaos_experiments_winner_fk
    FOREIGN KEY (workspace_id, winner_variant_id)
    REFERENCES viryaos_experiment_variants (workspace_id, id)
    ON DELETE RESTRICT;
CREATE INDEX viryaos_experiment_variants_running_idx
    ON viryaos_experiment_variants (workspace_id, experiment_id, active, id);

CREATE TABLE viryaos_experiment_observations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    experiment_id uuid NOT NULL,
    variant_id uuid NOT NULL,
    observation_key text NOT NULL CHECK (btrim(observation_key) <> '' AND char_length(observation_key) <= 200),
    exposures_delta integer NOT NULL CHECK (exposures_delta >= 0),
    conversions_delta integer NOT NULL CHECK (conversions_delta >= 0 AND conversions_delta <= exposures_delta),
    value_minor_delta bigint NOT NULL DEFAULT 0 CHECK (value_minor_delta >= 0),
    observed_at timestamptz NOT NULL,
    CONSTRAINT viryaos_experiment_observations_experiment_fk
        FOREIGN KEY (workspace_id, experiment_id)
        REFERENCES viryaos_experiments (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT viryaos_experiment_observations_variant_fk
        FOREIGN KEY (workspace_id, variant_id)
        REFERENCES viryaos_experiment_variants (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, experiment_id, observation_key)
);
CREATE INDEX viryaos_experiment_observations_time_idx
    ON viryaos_experiment_observations (workspace_id, experiment_id, observed_at DESC, id DESC);

-- A tiny explicit ledger for system-managed audience campaigns lets the domain
-- distinguish its own lifecycle phases from human-created communication work.
CREATE TABLE viryaos_campaign_lifecycle_emissions (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    phase text NOT NULL CHECK (phase IN ('announcement','interest_reminder','last_call','day_of','thank_you')),
    communication_campaign_id uuid NOT NULL,
    action_id uuid NOT NULL,
    emitted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, event_id, phase),
    CONSTRAINT viryaos_campaign_lifecycle_event_fk
        FOREIGN KEY (workspace_id, event_id) REFERENCES events (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_campaign_lifecycle_campaign_fk
        FOREIGN KEY (workspace_id, communication_campaign_id)
        REFERENCES communication_campaigns (workspace_id, id) ON DELETE RESTRICT,
    CONSTRAINT viryaos_campaign_lifecycle_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id) ON DELETE RESTRICT
);

-- Query-shape helpers. These support bounded batch scans that PostgreSQL 18 can
-- service through its asynchronous I/O path without application-side fan-out.
CREATE INDEX IF NOT EXISTS event_interests_event_fan_idx
    ON event_interests (workspace_id, event_id, fan_id);
CREATE INDEX IF NOT EXISTS communication_campaigns_autopilot_slug_idx
    ON communication_campaigns (workspace_id, slug, status, id);
CREATE INDEX IF NOT EXISTS inventory_reservation_items_variant_reservation_idx
    ON inventory_reservation_items (workspace_id, variant_id, reservation_id);
CREATE INDEX IF NOT EXISTS inventory_reservations_committed_time_idx
    ON inventory_reservations (workspace_id, committed_at DESC, id)
    WHERE status = 'committed' AND reservation_kind = 'order';

-- Ticket tier allocation is opt-in and operator bounded. The pricing domain may
-- only unlock explicit per-tier capacity inside these versioned guardrails.
CREATE TABLE viryaos_ticket_type_allocation_guardrails (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    ticket_type_id uuid NOT NULL,
    minimum_capacity integer NOT NULL CHECK (minimum_capacity BETWEEN 1 AND 1000000),
    maximum_capacity integer NOT NULL CHECK (maximum_capacity BETWEEN 1 AND 1000000),
    step_capacity integer NOT NULL CHECK (step_capacity BETWEEN 1 AND 1000000),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, ticket_type_id),
    CONSTRAINT viryaos_ticket_allocation_guardrail_type_fk
        FOREIGN KEY (workspace_id, ticket_type_id)
        REFERENCES ticket_types (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (minimum_capacity <= maximum_capacity),
    CHECK (step_capacity <= maximum_capacity)
);

CREATE TRIGGER viryaos_ticket_type_allocation_guardrails_set_updated_at
BEFORE UPDATE ON viryaos_ticket_type_allocation_guardrails
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_ticket_type_allocation_guardrail_history (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    ticket_type_id uuid NOT NULL,
    version bigint NOT NULL CHECK (version > 0),
    minimum_capacity integer NOT NULL,
    maximum_capacity integer NOT NULL,
    step_capacity integer NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, ticket_type_id, version),
    CONSTRAINT viryaos_ticket_allocation_guardrail_history_type_fk
        FOREIGN KEY (workspace_id, ticket_type_id)
        REFERENCES ticket_types (workspace_id, id)
        ON DELETE CASCADE
);
