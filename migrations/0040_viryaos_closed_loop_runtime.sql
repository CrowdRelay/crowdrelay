-- ViryaOS closed-loop autonomy runtime.
--
-- Adds the missing evidence/control layer between a deterministic CrowdRelay
-- decision and an external executor (n8n/provider): expiring approvals,
-- executor capability heartbeats, execution receipts, automatic policy guards,
-- a cross-context contact governor, release-state ledger and privacy-bounded RUM.

ALTER TABLE viryaos_autopilot_policies
    ADD COLUMN guarded_until timestamptz,
    ADD COLUMN guardrail_reason text CHECK (
        guardrail_reason IS NULL OR (btrim(guardrail_reason) <> '' AND char_length(guardrail_reason) <= 160)
    );

ALTER TABLE viryaos_autopilot_actions
    ADD COLUMN approval_expires_at timestamptz;

UPDATE viryaos_autopilot_actions
SET approval_expires_at = created_at + INTERVAL '72 hours'
WHERE status = 'awaiting_approval' AND approval_expires_at IS NULL;

CREATE INDEX viryaos_autopilot_actions_approval_expiry_idx
    ON viryaos_autopilot_actions (workspace_id, approval_expires_at, id)
    WHERE status = 'awaiting_approval';

ALTER TABLE viryaos_autopilot_measurements
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_measurements_measurement_kind_check;
ALTER TABLE viryaos_autopilot_measurements
    ADD CONSTRAINT viryaos_autopilot_measurements_measurement_kind_check CHECK (measurement_kind IN (
        'ticket_revenue_72h','merch_gross_proxy_7d','promotion_roas_7d',
        'booking_reply_7d','outreach_reply_7d','audience_ticket_revenue_72h'
    ));

-- Once at least one executor is registered for a workspace, capability-aware
-- mode is active permanently. This lets the migration deploy before n8n is
-- upgraded, while preventing silent legacy fallback after the first heartbeat.
CREATE TABLE viryaos_executor_instances (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    executor_id text NOT NULL CHECK (btrim(executor_id) <> '' AND char_length(executor_id) <= 120),
    version text NOT NULL CHECK (btrim(version) <> '' AND char_length(version) <= 80),
    manifest_sha text NOT NULL CHECK (btrim(manifest_sha) <> '' AND char_length(manifest_sha) <= 128),
    observed_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, executor_id),
    CHECK (expires_at > observed_at)
);

CREATE TRIGGER viryaos_executor_instances_set_updated_at
BEFORE UPDATE ON viryaos_executor_instances
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_executor_capabilities (
    workspace_id uuid NOT NULL,
    executor_id text NOT NULL,
    capability text NOT NULL CHECK (btrim(capability) <> '' AND char_length(capability) <= 120),
    capability_version text NOT NULL CHECK (
        btrim(capability_version) <> '' AND char_length(capability_version) <= 40
    ),
    observed_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, executor_id, capability),
    CONSTRAINT viryaos_executor_capabilities_instance_fk
        FOREIGN KEY (workspace_id, executor_id)
        REFERENCES viryaos_executor_instances (workspace_id, executor_id)
        ON DELETE CASCADE,
    CHECK (expires_at > observed_at)
);

CREATE INDEX viryaos_executor_capabilities_active_idx
    ON viryaos_executor_capabilities (workspace_id, capability, expires_at DESC, executor_id);

-- Runtime circuit breaker. Three executor failures inside 15 minutes pause all
-- capabilities from that executor for 15 minutes. Heartbeats do not clear the
-- guard; an observed success or expiry does.
CREATE TABLE viryaos_executor_circuit_breakers (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    executor_id text NOT NULL CHECK (btrim(executor_id) <> '' AND char_length(executor_id) <= 120),
    failure_count integer NOT NULL DEFAULT 0 CHECK (failure_count >= 0),
    last_failure_at timestamptz,
    guarded_until timestamptz,
    reason text CHECK (reason IS NULL OR (btrim(reason) <> '' AND char_length(reason) <= 96)),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, executor_id),
    CHECK (failure_count = 0 OR last_failure_at IS NOT NULL)
);

CREATE TRIGGER viryaos_executor_circuit_breakers_set_updated_at
BEFORE UPDATE ON viryaos_executor_circuit_breakers
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_executor_circuit_breakers_guard_idx
    ON viryaos_executor_circuit_breakers (workspace_id, guarded_until DESC, executor_id)
    WHERE guarded_until IS NOT NULL;

-- Provider-confirmed execution is evidence separate from the CrowdRelay action
-- state. `succeeded` on an action means the deterministic side effect/intent was
-- durably committed; this ledger says what the external executor/provider did.
CREATE TABLE viryaos_autopilot_execution_reports (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id uuid NOT NULL,
    receipt_key text NOT NULL CHECK (btrim(receipt_key) <> '' AND char_length(receipt_key) <= 200),
    executor_id text NOT NULL CHECK (btrim(executor_id) <> '' AND char_length(executor_id) <= 120),
    status text NOT NULL CHECK (status IN ('accepted','executing','succeeded','failed')),
    provider_reference text CHECK (
        provider_reference IS NULL OR char_length(provider_reference) <= 240
    ),
    error_kind text CHECK (error_kind IS NULL OR char_length(error_kind) <= 96),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    occurred_at timestamptz NOT NULL,
    received_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, receipt_key),
    UNIQUE (workspace_id, id),
    CONSTRAINT viryaos_autopilot_execution_reports_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX viryaos_autopilot_execution_reports_action_idx
    ON viryaos_autopilot_execution_reports (workspace_id, action_id, occurred_at DESC, id DESC);
CREATE INDEX viryaos_autopilot_execution_reports_status_idx
    ON viryaos_autopilot_execution_reports (workspace_id, status, occurred_at DESC, id DESC);

-- Shared outbound throttle across Booking + Outreach (and future contexts).
-- The row contains only normalized delivery address needed for the safety
-- invariant; it is never exposed by the operator overview.
CREATE TABLE viryaos_contact_governor (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    normalized_contact text NOT NULL CHECK (
        btrim(normalized_contact) <> '' AND char_length(normalized_contact) <= 320
    ),
    last_context text NOT NULL CHECK (btrim(last_context) <> '' AND char_length(last_context) <= 64),
    last_action_id uuid,
    last_outbound_at timestamptz NOT NULL,
    next_contact_after timestamptz NOT NULL,
    do_not_contact boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, normalized_contact),
    CONSTRAINT viryaos_contact_governor_action_fk
        FOREIGN KEY (workspace_id, last_action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (next_contact_after >= last_outbound_at)
);

CREATE TRIGGER viryaos_contact_governor_set_updated_at
BEFORE UPDATE ON viryaos_contact_governor
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

-- Production component ledger. CI/deploy/runtime reporters all write the same
-- provider-neutral shape. No deploy is triggered by this table.
CREATE TABLE viryaos_release_components (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    component_key text NOT NULL CHECK (btrim(component_key) <> '' AND char_length(component_key) <= 80),
    environment text NOT NULL DEFAULT 'production' CHECK (
        btrim(environment) <> '' AND char_length(environment) <= 40
    ),
    source_sha text NOT NULL CHECK (btrim(source_sha) <> '' AND char_length(source_sha) <= 128),
    artifact_digest text CHECK (artifact_digest IS NULL OR char_length(artifact_digest) <= 200),
    deploy_ref text CHECK (deploy_ref IS NULL OR char_length(deploy_ref) <= 240),
    version text CHECK (version IS NULL OR char_length(version) <= 80),
    manifest_sha text CHECK (manifest_sha IS NULL OR char_length(manifest_sha) <= 128),
    observed_at timestamptz NOT NULL,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, component_key, environment)
);

CREATE TRIGGER viryaos_release_components_set_updated_at
BEFORE UPDATE ON viryaos_release_components
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_release_components_observed_idx
    ON viryaos_release_components (workspace_id, environment, observed_at DESC, component_key);

-- Privacy-bounded first-party RUM. Deliberately no user id, IP, email, session
-- id or fingerprint columns. Samples are low-cardinality and short-lived.
CREATE TABLE viryaos_rum_samples (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    surface text NOT NULL CHECK (surface IN ('virya_www','synesthesia','virya_signal')),
    metric_key text NOT NULL CHECK (btrim(metric_key) <> '' AND char_length(metric_key) <= 80),
    value double precision NOT NULL CHECK (
        value >= 0 AND value < 'Infinity'::double precision
    ),
    route text CHECK (route IS NULL OR char_length(route) <= 160),
    device_class text CHECK (device_class IS NULL OR char_length(device_class) <= 40),
    release text CHECK (release IS NULL OR char_length(release) <= 128),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    observed_at timestamptz NOT NULL,
    received_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX viryaos_rum_samples_metric_time_idx
    ON viryaos_rum_samples (workspace_id, surface, metric_key, received_at DESC, id DESC);
CREATE INDEX viryaos_rum_samples_retention_idx
    ON viryaos_rum_samples (workspace_id, received_at, id);
