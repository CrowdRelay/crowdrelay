-- Fan-source attribution snapshots (fan source brief, FAN_SOURCE_BRIEF_PLAN.md).
--
-- attribute_fan_growth already computes where fan growth came from — observed,
-- incremental (counterfactual-adjusted), and durable 30-day survivors, broken
-- down per template, per strategy, and per evidence quality — and the answer
-- was thrown away into a log line. This table keeps it.
--
-- One row per workspace per hour, written by the worker at the end of an
-- autopilot cycle. The full serialized FanGrowthAttribution rides in
-- `attribution`; the four aggregate columns are denormalized so list and
-- trend reads never unpack the blob. `attribution` is the authority — the
-- columns exist so the operator surfaces stay cheap, not to be a second
-- source of truth.
--
-- No backfill: attribution only became channel-complete with migration 0343
-- (tracked links + anonymous_visitor_id on Telegram/Discord), so rows start
-- accumulating from the first post-deploy cycle. An empty table is the
-- honest answer for a workspace with no resolved evidence yet.
CREATE TABLE fan_source_snapshots (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    captured_at timestamptz NOT NULL DEFAULT now(),
    attribution jsonb NOT NULL CHECK (jsonb_typeof(attribution) = 'object'),
    total_observed_fans double precision NOT NULL,
    total_incremental_fans double precision NOT NULL,
    total_durable_fans double precision NOT NULL,
    resolved_observations integer NOT NULL,
    PRIMARY KEY (workspace_id, captured_at)
);
