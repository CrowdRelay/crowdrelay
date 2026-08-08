-- Audience Intelligence + communication intent plane.
--
-- Additive by design: this migration does not modify fan lifecycle, ticketing,
-- mail delivery, webhook delivery, n8n workflows, or existing outbox semantics.
-- Communication scheduling reuses outbox_events.available_at; adapters resolve
-- recipients later through a privileged delivery-plan endpoint.

INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
SELECT id, 'communication_campaigns_enabled', false, 'staged rollout'
FROM workspaces
ON CONFLICT (workspace_id, key) DO NOTHING;

CREATE TABLE audience_segments (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    name text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 160),
    description text CHECK (description IS NULL OR char_length(description) <= 1000),
    filter jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(filter) = 'object'),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug)
);

CREATE TRIGGER audience_segments_set_updated_at
BEFORE UPDATE ON audience_segments
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX audience_segments_active_idx
    ON audience_segments (workspace_id, active, name, id);

CREATE TABLE fan_audience_tags (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    tag text NOT NULL CHECK (tag ~ '^[a-z0-9][a-z0-9:_-]{0,63}$'),
    source text NOT NULL DEFAULT 'operator'
        CHECK (source IN ('operator', 'system', 'import')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, fan_id, tag),
    CONSTRAINT fan_audience_tags_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX fan_audience_tags_tag_idx
    ON fan_audience_tags (workspace_id, tag, fan_id);

CREATE TABLE communication_campaigns (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    segment_id uuid NOT NULL,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    name text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 160),
    channel text NOT NULL CHECK (channel IN ('email', 'push', 'in_app')),
    template_key text NOT NULL
        CHECK (btrim(template_key) <> '' AND char_length(template_key) <= 160),
    subject text CHECK (subject IS NULL OR char_length(subject) <= 240),
    content jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(content) = 'object'),
    status text NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'scheduled', 'completed', 'cancelled')),
    scheduled_at timestamptz,
    dispatch_event_id uuid,
    recipient_snapshot_at timestamptz,
    recipient_snapshot_count integer
        CHECK (recipient_snapshot_count IS NULL OR recipient_snapshot_count >= 0),
    recipient_count integer CHECK (recipient_count IS NULL OR recipient_count >= 0),
    delivered_count integer CHECK (delivered_count IS NULL OR delivered_count >= 0),
    failed_count integer CHECK (failed_count IS NULL OR failed_count >= 0),
    completed_at timestamptz,
    cancelled_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug),
    CONSTRAINT communication_campaigns_segment_fk
        FOREIGN KEY (workspace_id, segment_id)
        REFERENCES audience_segments (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT communication_campaigns_dispatch_event_fk
        FOREIGN KEY (workspace_id, dispatch_event_id)
        REFERENCES outbox_events (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (
        (status = 'draft'
            AND scheduled_at IS NULL
            AND dispatch_event_id IS NULL
            AND recipient_count IS NULL
            AND delivered_count IS NULL
            AND failed_count IS NULL
            AND completed_at IS NULL
            AND cancelled_at IS NULL)
        OR (status = 'scheduled'
            AND scheduled_at IS NOT NULL
            AND dispatch_event_id IS NOT NULL
            AND recipient_count IS NULL
            AND delivered_count IS NULL
            AND failed_count IS NULL
            AND completed_at IS NULL
            AND cancelled_at IS NULL)
        OR (status = 'completed'
            AND scheduled_at IS NOT NULL
            AND dispatch_event_id IS NOT NULL
            AND recipient_count IS NOT NULL
            AND delivered_count IS NOT NULL
            AND failed_count IS NOT NULL
            AND delivered_count + failed_count = recipient_count
            AND completed_at IS NOT NULL
            AND cancelled_at IS NULL)
        OR (status = 'cancelled'
            AND completed_at IS NULL
            AND cancelled_at IS NOT NULL)
    )
);

CREATE TRIGGER communication_campaigns_set_updated_at
BEFORE UPDATE ON communication_campaigns
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX communication_campaigns_status_due_idx
    ON communication_campaigns (workspace_id, status, scheduled_at, id)
    WHERE status = 'scheduled';

-- Stable membership snapshot for paginated dispatch. Only fan IDs are copied:
-- PII stays canonical in `fans`, and every delivery-plan page re-checks current
-- active status + latest marketing consent before exposing a recipient.
CREATE TABLE communication_campaign_recipients (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    campaign_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    snapshotted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, campaign_id, fan_id),
    CONSTRAINT communication_campaign_recipients_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES communication_campaigns (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT communication_campaign_recipients_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);

-- Fan 360 / segment preview hot-path helpers. These are additive and match
-- existing access patterns; no materialized copy of fan PII is introduced.
CREATE INDEX IF NOT EXISTS fan_acquisition_events_source_time_idx
    ON fan_acquisition_events (workspace_id, source, occurred_at DESC, fan_id);

CREATE INDEX IF NOT EXISTS ticket_orders_paid_buyer_idx
    ON ticket_orders (workspace_id, buyer_email, paid_at DESC, id)
    WHERE status IN ('paid', 'partially_refunded', 'refunded');

CREATE INDEX IF NOT EXISTS admission_passes_fan_status_idx
    ON admission_passes (workspace_id, fan_id, status, redeemed_at DESC, id);

CREATE INDEX IF NOT EXISTS event_interests_fan_event_idx
    ON event_interests (workspace_id, fan_id, event_id);

CREATE INDEX IF NOT EXISTS synesthesia_reward_entries_fan_time_idx
    ON synesthesia_reward_entries (workspace_id, fan_id, entered_at DESC, id);
