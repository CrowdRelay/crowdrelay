-- External growth metrics: normalized snapshots from platforms CrowdRelay does
-- not own (streaming, video, listings, social, website, ticketing).
--
-- Two tables only. `viryaos_growth_metric_series` is the operator-declared
-- identity of one tracked number; `viryaos_growth_metric_points` is the
-- append-only observation log for that identity. Trend, baseline and anomaly
-- are derived in the domain from the points, never stored as a second truth,
-- so a re-ingest or a provider backfill cannot leave a stale derived row
-- behind. Nothing here executes anything: the Autopilot `growth_metrics`
-- context turns a detected anomaly into a normal decision + action under the
-- existing authority, quota and idempotency rules.

CREATE TABLE viryaos_growth_metric_series (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    platform text NOT NULL CHECK (platform IN (
        'spotify', 'youtube', 'bandsintown', 'social', 'website', 'ticketing', 'signal', 'merch'
    )),
    -- Provider-neutral metric name, e.g. 'followers', 'monthly_listeners',
    -- 'subscribers', 'views', 'trackers', 'sessions', 'orders'.
    metric_key text NOT NULL CHECK (btrim(metric_key) <> '' AND char_length(metric_key) <= 64),
    -- Optional first-party subject this series belongs to (an event, a city,
    -- a release). Kept as a loose reference on purpose: the series is external
    -- evidence, so it must not block deletion of the business row it annotates.
    subject_kind text CHECK (subject_kind IS NULL OR subject_kind IN (
        'event', 'city', 'release_plan', 'content_source', 'beacon'
    )),
    subject_id uuid,
    display_name text NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 120),
    -- Whether a rising number is good. Refunds, unsubscribes and churn are
    -- tracked with 'lower_is_better' so the same trend code fits both.
    direction text NOT NULL DEFAULT 'higher_is_better'
        CHECK (direction IN ('higher_is_better', 'lower_is_better')),
    -- How close the metric sits to real value. The opportunity engine
    -- deliberately down-weights 'vanity' so a follower spike never outranks a
    -- ticket or merch problem.
    value_tier text NOT NULL DEFAULT 'intermediate'
        CHECK (value_tier IN ('vanity', 'intermediate', 'downstream')),
    -- Expected reporting cadence. Used to detect a dead feed (growth debt)
    -- rather than to schedule anything.
    expected_interval_hours integer NOT NULL DEFAULT 24
        CHECK (expected_interval_hours BETWEEN 1 AND 720),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, platform, metric_key, subject_kind, subject_id),
    CHECK ((subject_kind IS NULL) = (subject_id IS NULL))
);

CREATE TRIGGER viryaos_growth_metric_series_set_updated_at
BEFORE UPDATE ON viryaos_growth_metric_series
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_growth_metric_series_active_idx
    ON viryaos_growth_metric_series (workspace_id, active, platform, metric_key);

-- Append-only observations. `captured_at` is the provider's own observation
-- time, so re-delivering the same snapshot is a no-op and out-of-order
-- delivery still lands in the right place on the timeline.
CREATE TABLE viryaos_growth_metric_points (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    series_id uuid NOT NULL,
    captured_at timestamptz NOT NULL,
    -- Absolute level of the metric at `captured_at`, never a delta: deltas are
    -- derived, and storing them would make a missed snapshot unrecoverable.
    value bigint NOT NULL CHECK (value >= 0),
    source text NOT NULL CHECK (btrim(source) <> '' AND char_length(source) <= 64),
    ingested_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_growth_metric_points_series_fk
        FOREIGN KEY (workspace_id, series_id)
        REFERENCES viryaos_growth_metric_series (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, series_id, captured_at)
);

CREATE INDEX viryaos_growth_metric_points_window_idx
    ON viryaos_growth_metric_points (workspace_id, series_id, captured_at DESC, id DESC);

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics'
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
        'growth_metrics'
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
        'growth_metrics'
    ));

-- Provisioned disabled and at 'observe' like every other context: a new
-- detector must be watched before it is allowed to enqueue anything.
INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT id, 'growth_metrics', 12 FROM workspaces
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
        (NEW.id, 'show_growth', 14),
        (NEW.id, 'growth_metrics', 12)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;
