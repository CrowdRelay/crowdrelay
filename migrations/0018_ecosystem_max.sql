-- End-to-end operating controls for the Virya/CrowdRelay ecosystem.
-- The migration is additive and keeps public fan/ticket paths independent from
-- the operator dashboard.

CREATE TABLE ecosystem_feature_flags (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    key text NOT NULL CHECK (key ~ '^[a-z][a-z0-9_.-]{2,63}$'),
    enabled boolean NOT NULL,
    reason text CHECK (reason IS NULL OR char_length(reason) <= 500),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by_request_id text CHECK (
        updated_by_request_id IS NULL OR char_length(updated_by_request_id) <= 128
    ),
    PRIMARY KEY (workspace_id, key)
);

CREATE INDEX ecosystem_feature_flags_updated_idx
    ON ecosystem_feature_flags (workspace_id, updated_at DESC, key);

INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
SELECT workspace.id, defaults.key, defaults.enabled, 'bootstrap default'
FROM workspaces AS workspace
CROSS JOIN (VALUES
    ('ticket_sales_enabled', true),
    ('ticket_delivery_enabled', true),
    ('gate_redemption_enabled', true),
    ('mailer_enabled', true),
    ('meta_publish_enabled', true),
    ('bandsintown_sync_enabled', true),
    ('n8n_ingress_enabled', true),
    ('automatic_retry_enabled', true)
) AS defaults(key, enabled)
ON CONFLICT (workspace_id, key) DO NOTHING;

CREATE TABLE reconciliation_runs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    status text NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
    trigger text NOT NULL CHECK (trigger IN ('manual', 'scheduled', 'deploy', 'restore_drill')),
    request_id text CHECK (request_id IS NULL OR char_length(request_id) <= 128),
    finding_count integer NOT NULL DEFAULT 0 CHECK (finding_count >= 0),
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    UNIQUE (workspace_id, id),
    CHECK ((status = 'running') = (finished_at IS NULL))
);

CREATE INDEX reconciliation_runs_recent_idx
    ON reconciliation_runs (workspace_id, started_at DESC, id DESC);

CREATE TABLE reconciliation_findings (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    run_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind ~ '^[a-z][a-z0-9_.-]{2,95}$'),
    severity text NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    entity_type text NOT NULL CHECK (btrim(entity_type) <> '' AND char_length(entity_type) <= 64),
    entity_id uuid,
    entity_label text CHECK (entity_label IS NULL OR char_length(entity_label) <= 160),
    summary text NOT NULL CHECK (btrim(summary) <> '' AND char_length(summary) <= 500),
    suggested_action text CHECK (suggested_action IS NULL OR char_length(suggested_action) <= 96),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    UNIQUE (workspace_id, id),
    CONSTRAINT reconciliation_findings_run_fk
        FOREIGN KEY (workspace_id, run_id)
        REFERENCES reconciliation_runs (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX reconciliation_findings_open_idx
    ON reconciliation_findings (workspace_id, severity, created_at DESC, id DESC)
    WHERE resolved_at IS NULL;
CREATE INDEX reconciliation_findings_run_idx
    ON reconciliation_findings (workspace_id, run_id, created_at, id);

CREATE TABLE show_checklist_items (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    item_key text NOT NULL CHECK (item_key ~ '^[a-z][a-z0-9_.-]{2,63}$'),
    status text NOT NULL CHECK (status IN ('pending', 'done', 'blocked', 'skipped')),
    note text CHECK (note IS NULL OR char_length(note) <= 1000),
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by_request_id text CHECK (
        updated_by_request_id IS NULL OR char_length(updated_by_request_id) <= 128
    ),
    PRIMARY KEY (workspace_id, event_id, item_key),
    CONSTRAINT show_checklist_items_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX show_checklist_items_status_idx
    ON show_checklist_items (workspace_id, event_id, status, item_key);

CREATE TABLE show_notification_emissions (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    phase text NOT NULL CHECK (phase IN ('week', 'day', 'gate', 'followup')),
    emitted_at timestamptz NOT NULL DEFAULT now(),
    outbox_event_id uuid NOT NULL,
    PRIMARY KEY (workspace_id, event_id, phase),
    CONSTRAINT show_notification_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT show_notification_outbox_fk
        FOREIGN KEY (workspace_id, outbox_event_id)
        REFERENCES outbox_events (workspace_id, id)
        ON DELETE RESTRICT
);

-- New workspaces created after this migration receive the defaults through the
-- API's lazy initialization path. Existing workspaces are seeded above.
