-- ViryaOS deterministic Autopilot control plane.
--
-- The schema is intentionally provider-agnostic: bounded contexts produce
-- explainable decisions, policies decide how much authority is allowed, and
-- durable action jobs execute through existing CrowdRelay/n8n boundaries.
-- Defaults are OFF and require an explicit operator rollout.

CREATE TABLE viryaos_autopilot_policies (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    context text NOT NULL CHECK (context IN (
        'ticket_yield', 'fan_lifecycle', 'merchandising', 'merch_pricing', 'booking_opportunity', 'promotion_budget'
    )),
    enabled boolean NOT NULL DEFAULT false,
    autonomy_level text NOT NULL DEFAULT 'observe' CHECK (autonomy_level IN (
        'observe', 'recommend', 'require_approval', 'bounded_auto'
    )),
    minimum_confidence_basis_points integer NOT NULL DEFAULT 8000
        CHECK (minimum_confidence_basis_points BETWEEN 0 AND 10000),
    max_actions_24h integer NOT NULL DEFAULT 50 CHECK (max_actions_24h BETWEEN 1 AND 1000),
    config jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(config) = 'object'),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, context)
);

CREATE TRIGGER viryaos_autopilot_policies_set_updated_at
BEFORE UPDATE ON viryaos_autopilot_policies
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT workspace.id, context.name, context.max_actions_24h
FROM workspaces AS workspace
CROSS JOIN (VALUES
    ('ticket_yield', 10),
    ('fan_lifecycle', 100),
    ('merchandising', 20),
    ('merch_pricing', 10),
    ('booking_opportunity', 10),
    ('promotion_budget', 20)
) AS context(name, max_actions_24h)
ON CONFLICT (workspace_id, context) DO NOTHING;

CREATE FUNCTION viryaos_provision_autopilot_policies()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
    VALUES
        (NEW.id, 'ticket_yield', 10),
        (NEW.id, 'fan_lifecycle', 100),
        (NEW.id, 'merchandising', 20),
        (NEW.id, 'merch_pricing', 10),
        (NEW.id, 'booking_opportunity', 10),
        (NEW.id, 'promotion_budget', 20)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;

CREATE TRIGGER viryaos_workspaces_provision_autopilot
AFTER INSERT ON workspaces
FOR EACH ROW
EXECUTE FUNCTION viryaos_provision_autopilot_policies();

-- Provider-neutral state snapshots are written by integration adapters (for
-- example n8n/Meta) and consumed by the pure Promotion bounded context. The
-- adapter reports facts; it never decides the budget.
CREATE TABLE viryaos_promotion_campaign_states (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    provider text NOT NULL CHECK (btrim(provider) <> '' AND char_length(provider) <= 32),
    external_campaign_key text NOT NULL
        CHECK (btrim(external_campaign_key) <> '' AND char_length(external_campaign_key) <= 160),
    event_id uuid,
    currency text NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    current_daily_budget_minor bigint NOT NULL CHECK (current_daily_budget_minor > 0),
    minimum_daily_budget_minor bigint NOT NULL CHECK (minimum_daily_budget_minor > 0),
    maximum_daily_budget_minor bigint NOT NULL CHECK (maximum_daily_budget_minor > 0),
    spend_last_7d_minor bigint NOT NULL CHECK (spend_last_7d_minor >= 0),
    spend_month_to_date_minor bigint NOT NULL DEFAULT 0 CHECK (spend_month_to_date_minor >= 0),
    attributed_revenue_last_7d_minor bigint NOT NULL CHECK (attributed_revenue_last_7d_minor >= 0),
    active boolean NOT NULL DEFAULT true,
    last_budget_change_at timestamptz,
    observed_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, provider, external_campaign_key),
    CONSTRAINT viryaos_promotion_campaign_event_fk
        FOREIGN KEY (workspace_id, event_id) REFERENCES events (workspace_id, id) ON DELETE SET NULL (event_id),
    CHECK (minimum_daily_budget_minor <= current_daily_budget_minor),
    CHECK (current_daily_budget_minor <= maximum_daily_budget_minor),
    CHECK (expires_at > observed_at)
);

CREATE TRIGGER viryaos_promotion_campaign_states_set_updated_at
BEFORE UPDATE ON viryaos_promotion_campaign_states
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_promotion_campaign_states_active_idx
    ON viryaos_promotion_campaign_states (workspace_id, expires_at, observed_at DESC, id)
    WHERE active;

CREATE TABLE viryaos_promotion_campaign_observations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    campaign_id uuid NOT NULL,
    current_daily_budget_minor bigint NOT NULL CHECK (current_daily_budget_minor > 0),
    spend_last_7d_minor bigint NOT NULL CHECK (spend_last_7d_minor >= 0),
    spend_month_to_date_minor bigint NOT NULL DEFAULT 0 CHECK (spend_month_to_date_minor >= 0),
    attributed_revenue_last_7d_minor bigint NOT NULL CHECK (attributed_revenue_last_7d_minor >= 0),
    active boolean NOT NULL,
    observed_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    CONSTRAINT viryaos_promotion_campaign_observations_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES viryaos_promotion_campaign_states (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, campaign_id, observed_at),
    CHECK (expires_at > observed_at)
);

CREATE INDEX viryaos_promotion_campaign_observations_time_idx
    ON viryaos_promotion_campaign_observations (workspace_id, campaign_id, observed_at DESC, id DESC);

-- Normalized external market observations. Integrations report bounded facts;
-- only deterministic bounded contexts can turn them into actions.
CREATE TABLE viryaos_city_market_signals (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    source text NOT NULL CHECK (btrim(source) <> '' AND char_length(source) <= 64),
    city_id uuid NOT NULL REFERENCES cities(id) ON DELETE CASCADE,
    signal_kind text NOT NULL CHECK (signal_kind IN (
        'streaming_momentum', 'search_interest', 'social_momentum', 'live_demand'
    )),
    score_basis_points integer NOT NULL CHECK (score_basis_points BETWEEN 0 AND 10000),
    confidence_basis_points integer NOT NULL CHECK (confidence_basis_points BETWEEN 0 AND 10000),
    observed_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, source, city_id, signal_kind),
    CHECK (expires_at > observed_at)
);

CREATE TRIGGER viryaos_city_market_signals_set_updated_at
BEFORE UPDATE ON viryaos_city_market_signals
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_city_market_signals_active_idx
    ON viryaos_city_market_signals (workspace_id, city_id, expires_at, observed_at DESC, id);

CREATE TABLE viryaos_city_market_signal_observations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    signal_id uuid NOT NULL,
    score_basis_points integer NOT NULL CHECK (score_basis_points BETWEEN 0 AND 10000),
    confidence_basis_points integer NOT NULL CHECK (confidence_basis_points BETWEEN 0 AND 10000),
    observed_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    CONSTRAINT viryaos_city_market_signal_observations_signal_fk
        FOREIGN KEY (workspace_id, signal_id)
        REFERENCES viryaos_city_market_signals (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, signal_id, observed_at),
    CHECK (expires_at > observed_at)
);

CREATE INDEX viryaos_city_market_signal_observations_time_idx
    ON viryaos_city_market_signal_observations (workspace_id, signal_id, observed_at DESC, id DESC);

-- Verified booking targets are operator-owned commercial relationships. External
-- intelligence may discover candidates, but only rows promoted here are eligible
-- for autonomous outreach. The domain never asks n8n to choose a recipient.
CREATE TABLE viryaos_booking_targets (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    city_id uuid NOT NULL REFERENCES cities(id) ON DELETE RESTRICT,
    target_kind text NOT NULL CHECK (target_kind IN ('venue', 'promoter', 'festival')),
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 200),
    contact_email text NOT NULL CHECK (
        char_length(contact_email) <= 320
        AND contact_email ~* '^[^[:space:]@]+@[^[:space:]@]+\.[^[:space:]@]+$'
    ),
    priority integer NOT NULL DEFAULT 50 CHECK (priority BETWEEN 0 AND 100),
    relationship_score integer NOT NULL DEFAULT 50 CHECK (relationship_score BETWEEN 0 AND 100),
    active boolean NOT NULL DEFAULT true,
    accepts_booking boolean NOT NULL DEFAULT true,
    last_outreach_at timestamptz,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, city_id, contact_email)
);

CREATE TRIGGER viryaos_booking_targets_set_updated_at
BEFORE UPDATE ON viryaos_booking_targets
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_booking_targets_eligible_idx
    ON viryaos_booking_targets (workspace_id, city_id, priority DESC, relationship_score DESC, id)
    WHERE active AND accepts_booking;

CREATE TABLE viryaos_booking_target_history (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_id uuid NOT NULL,
    version bigint NOT NULL CHECK (version > 0),
    target_kind text NOT NULL,
    display_name text NOT NULL,
    contact_email text NOT NULL,
    priority integer NOT NULL,
    relationship_score integer NOT NULL,
    active boolean NOT NULL,
    accepts_booking boolean NOT NULL,
    changed_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_booking_target_history_target_fk
        FOREIGN KEY (workspace_id, target_id)
        REFERENCES viryaos_booking_targets (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, target_id, version)
);

-- Product-specific price/cost guardrails are operator-owned economics, not
-- inferred market facts. They are required before Merch Yield can act.
CREATE TABLE viryaos_merch_product_economics (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    product_id uuid NOT NULL,
    minimum_price_minor bigint NOT NULL CHECK (minimum_price_minor >= 0),
    maximum_price_minor bigint NOT NULL CHECK (maximum_price_minor >= minimum_price_minor),
    unit_cost_minor bigint CHECK (unit_cost_minor IS NULL OR unit_cost_minor >= 0),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, product_id),
    CONSTRAINT viryaos_merch_product_economics_product_fk
        FOREIGN KEY (workspace_id, product_id)
        REFERENCES merch_products (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (unit_cost_minor IS NULL OR unit_cost_minor <= maximum_price_minor)
);

CREATE TRIGGER viryaos_merch_product_economics_set_updated_at
BEFORE UPDATE ON viryaos_merch_product_economics
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_merch_product_economics_history (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    product_id uuid NOT NULL,
    minimum_price_minor bigint NOT NULL CHECK (minimum_price_minor >= 0),
    maximum_price_minor bigint NOT NULL CHECK (maximum_price_minor >= minimum_price_minor),
    unit_cost_minor bigint CHECK (unit_cost_minor IS NULL OR unit_cost_minor >= 0),
    version bigint NOT NULL CHECK (version > 0),
    recorded_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_merch_product_economics_history_product_fk
        FOREIGN KEY (workspace_id, product_id)
        REFERENCES merch_products (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, product_id, version),
    CHECK (unit_cost_minor IS NULL OR unit_cost_minor <= maximum_price_minor)
);

CREATE INDEX viryaos_merch_product_economics_history_idx
    ON viryaos_merch_product_economics_history (workspace_id, product_id, version DESC, id DESC);

CREATE TABLE viryaos_autopilot_decisions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    decision_key text NOT NULL CHECK (btrim(decision_key) <> '' AND char_length(decision_key) <= 240),
    context text NOT NULL CHECK (context IN (
        'ticket_yield', 'fan_lifecycle', 'merchandising', 'merch_pricing', 'booking_opportunity', 'promotion_budget'
    )),
    subject_kind text NOT NULL CHECK (btrim(subject_kind) <> '' AND char_length(subject_kind) <= 64),
    subject_id uuid NOT NULL,
    decision_kind text NOT NULL CHECK (btrim(decision_kind) <> '' AND char_length(decision_kind) <= 96),
    confidence_basis_points integer NOT NULL CHECK (confidence_basis_points BETWEEN 0 AND 10000),
    disposition text NOT NULL CHECK (disposition IN (
        'observe_only', 'recommend_only', 'require_approval', 'auto_execute', 'deny'
    )),
    reason text NOT NULL CHECK (btrim(reason) <> '' AND char_length(reason) <= 240),
    input_snapshot jsonb NOT NULL CHECK (jsonb_typeof(input_snapshot) = 'object'),
    policy_snapshot jsonb NOT NULL CHECK (jsonb_typeof(policy_snapshot) = 'object'),
    recommendation jsonb NOT NULL CHECK (jsonb_typeof(recommendation) = 'object'),
    evaluated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, decision_key)
);

CREATE INDEX viryaos_autopilot_decisions_context_time_idx
    ON viryaos_autopilot_decisions (workspace_id, context, evaluated_at DESC, id DESC);
CREATE INDEX viryaos_autopilot_decisions_subject_time_idx
    ON viryaos_autopilot_decisions (workspace_id, subject_kind, subject_id, evaluated_at DESC, id DESC);

CREATE TABLE viryaos_autopilot_actions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    decision_id uuid NOT NULL,
    context text NOT NULL CHECK (context IN (
        'ticket_yield', 'fan_lifecycle', 'merchandising', 'merch_pricing', 'booking_opportunity', 'promotion_budget'
    )),
    action_kind text NOT NULL CHECK (btrim(action_kind) <> '' AND char_length(action_kind) <= 96),
    subject_kind text NOT NULL CHECK (btrim(subject_kind) <> '' AND char_length(subject_kind) <= 64),
    subject_id uuid NOT NULL,
    idempotency_key text NOT NULL CHECK (btrim(idempotency_key) <> '' AND char_length(idempotency_key) <= 200),
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    status text NOT NULL CHECK (status IN (
        'awaiting_approval', 'queued', 'processing', 'succeeded', 'failed', 'cancelled'
    )),
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    available_at timestamptz NOT NULL DEFAULT now(),
    approved_at timestamptz,
    approved_by text CHECK (approved_by IS NULL OR char_length(approved_by) <= 200),
    started_at timestamptz,
    finished_at timestamptz,
    last_error_kind text CHECK (last_error_kind IS NULL OR char_length(last_error_kind) <= 96),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, idempotency_key),
    CONSTRAINT viryaos_autopilot_actions_decision_fk
        FOREIGN KEY (workspace_id, decision_id)
        REFERENCES viryaos_autopilot_decisions (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK ((status = 'awaiting_approval') OR approved_at IS NOT NULL OR approved_by IS NULL),
    CHECK ((status NOT IN ('succeeded', 'failed', 'cancelled')) OR finished_at IS NOT NULL)
);

CREATE TRIGGER viryaos_autopilot_actions_set_updated_at
BEFORE UPDATE ON viryaos_autopilot_actions
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_autopilot_actions_due_idx
    ON viryaos_autopilot_actions (available_at, id)
    WHERE status = 'queued';
CREATE INDEX viryaos_autopilot_actions_approval_idx
    ON viryaos_autopilot_actions (workspace_id, created_at DESC, id DESC)
    WHERE status = 'awaiting_approval';
CREATE INDEX viryaos_autopilot_actions_recent_idx
    ON viryaos_autopilot_actions (workspace_id, created_at DESC, id DESC);
CREATE UNIQUE INDEX viryaos_autopilot_actions_inflight_subject_uidx
    ON viryaos_autopilot_actions (workspace_id, context, action_kind, subject_id)
    WHERE status IN ('awaiting_approval', 'queued', 'processing');

CREATE TABLE viryaos_autopilot_action_attempts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id uuid NOT NULL,
    attempt_number integer NOT NULL CHECK (attempt_number > 0),
    outcome text NOT NULL CHECK (outcome IN ('started', 'succeeded', 'failed', 'skipped_idempotent')),
    error_kind text CHECK (error_kind IS NULL OR char_length(error_kind) <= 96),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    occurred_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_autopilot_action_attempts_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX viryaos_autopilot_action_attempts_action_idx
    ON viryaos_autopilot_action_attempts (workspace_id, action_id, occurred_at DESC, id DESC);

-- Exactly-once emission ledger for external side effects. An action may be
-- retried after a worker crash, but the same emission key cannot be created
-- twice and therefore cannot enqueue the same n8n/outbox intent twice.
CREATE TABLE viryaos_autopilot_action_emissions (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id uuid NOT NULL,
    emission_key text NOT NULL CHECK (btrim(emission_key) <> '' AND char_length(emission_key) <= 200),
    outbox_event_id uuid,
    emitted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, emission_key),
    CONSTRAINT viryaos_autopilot_action_emissions_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT viryaos_autopilot_action_emissions_outbox_fk
        FOREIGN KEY (workspace_id, outbox_event_id)
        REFERENCES outbox_events (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE TABLE viryaos_autopilot_measurements (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id uuid NOT NULL,
    measurement_kind text NOT NULL CHECK (measurement_kind IN (
        'ticket_revenue_72h', 'merch_gross_proxy_7d', 'promotion_roas_7d'
    )),
    subject_id uuid NOT NULL,
    action_finished_at timestamptz NOT NULL,
    baseline_value double precision NOT NULL CHECK (isfinite(baseline_value) AND baseline_value >= 0),
    due_at timestamptz NOT NULL,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'processing', 'succeeded', 'failed')),
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    available_at timestamptz NOT NULL,
    started_at timestamptz,
    finished_at timestamptz,
    last_error_kind text CHECK (last_error_kind IS NULL OR char_length(last_error_kind) <= 96),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, action_id, measurement_kind),
    CONSTRAINT viryaos_autopilot_measurements_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE CASCADE,
    CHECK ((status NOT IN ('succeeded', 'failed')) OR finished_at IS NOT NULL)
);

CREATE TRIGGER viryaos_autopilot_measurements_set_updated_at
BEFORE UPDATE ON viryaos_autopilot_measurements
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_autopilot_measurements_due_idx
    ON viryaos_autopilot_measurements (available_at, due_at, id)
    WHERE status = 'pending';

CREATE TABLE viryaos_autopilot_outcomes (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    decision_id uuid NOT NULL,
    action_id uuid,
    measurement_id uuid,
    metric_key text NOT NULL CHECK (btrim(metric_key) <> '' AND char_length(metric_key) <= 96),
    observed_value double precision NOT NULL CHECK (isfinite(observed_value)),
    baseline_value double precision CHECK (baseline_value IS NULL OR isfinite(baseline_value)),
    effect_assessment text CHECK (effect_assessment IS NULL OR effect_assessment IN ('improved', 'neutral', 'worsened')),
    delta_basis_points integer,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    observed_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_autopilot_outcomes_decision_fk
        FOREIGN KEY (workspace_id, decision_id)
        REFERENCES viryaos_autopilot_decisions (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT viryaos_autopilot_outcomes_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT viryaos_autopilot_outcomes_measurement_fk
        FOREIGN KEY (workspace_id, measurement_id)
        REFERENCES viryaos_autopilot_measurements (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (
        (measurement_id IS NULL AND effect_assessment IS NULL AND delta_basis_points IS NULL)
        OR (measurement_id IS NOT NULL AND effect_assessment IS NOT NULL AND delta_basis_points IS NOT NULL)
    )
);

CREATE INDEX viryaos_autopilot_outcomes_decision_idx
    ON viryaos_autopilot_outcomes (workspace_id, decision_id, observed_at DESC, id DESC);
CREATE UNIQUE INDEX viryaos_autopilot_outcomes_action_metric_uidx
    ON viryaos_autopilot_outcomes (workspace_id, action_id, metric_key)
    WHERE action_id IS NOT NULL;
CREATE UNIQUE INDEX viryaos_autopilot_outcomes_measurement_uidx
    ON viryaos_autopilot_outcomes (workspace_id, measurement_id)
    WHERE measurement_id IS NOT NULL;

-- Fast first-party snapshot access. These are selective partial indexes for the
-- exact facts the deterministic bounded contexts consume.
CREATE INDEX IF NOT EXISTS ticket_orders_autopilot_paid_time_idx
    ON ticket_orders (workspace_id, ticket_sale_id, paid_at DESC, id)
    WHERE status = 'paid';
CREATE INDEX IF NOT EXISTS inventory_ledger_autopilot_sales_idx
    ON inventory_ledger (workspace_id, variant_id, occurred_at DESC, id DESC)
    WHERE movement_kind = 'sale';
CREATE INDEX IF NOT EXISTS synesthesia_reward_entries_autopilot_fan_idx
    ON synesthesia_reward_entries (workspace_id, fan_id, entered_at DESC, run_id);

CREATE INDEX IF NOT EXISTS communication_campaign_recipients_autopilot_fan_idx
    ON communication_campaign_recipients (workspace_id, fan_id, snapshotted_at DESC, campaign_id);
CREATE INDEX IF NOT EXISTS viryaos_autopilot_actions_subject_history_idx
    ON viryaos_autopilot_actions (workspace_id, subject_id, action_kind, finished_at DESC, id DESC)
    WHERE status = 'succeeded';
