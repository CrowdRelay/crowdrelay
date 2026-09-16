-- Daily briefing: the cadence is something the system issues.
--
-- The autopilot already expires stale suggestions and caps how many are
-- outstanding; what it did not do is speak on a schedule. This migration
-- creates the artifact the schedule produces: one briefing per workspace
-- per tenant-local day, composed by the deterministic sweep from the rows
-- the band already owns — the active arc, the asks awaiting a decision,
-- the open handoffs, the coming production days and what landed in the
-- last 24 hours. The day the system has nothing to say, the briefing
-- says so — a calm cadence is what keeps "one briefing" from turning
-- back into N notifications.
--
-- Delivery rides the proven team-handoff path: each active member gets a
-- 'daily_briefing' assignment pointing at the briefing row, so the same
-- routing, email and trace machinery that carries every other handoff
-- carries this one. Briefing assignments ask to be read, not done —
-- they take no reminders and are cancelled when the next day's briefing
-- supersedes them.

CREATE TABLE viryaos_daily_briefings (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The tenant-local date the briefing speaks for. The sweep derives it
    -- from crew_timezone, not from the session clock — the same day must
    -- produce the same briefing no matter which zone the database runs in.
    local_date date NOT NULL,
    title text NOT NULL CHECK (btrim(title) <> '' AND char_length(title) <= 200),
    body text NOT NULL CHECK (btrim(body) <> '' AND char_length(body) <= 4000),
    -- What the composer found per section, as counts — {"pending_asks":2,
    -- "open_tasks":3, ...}. The body is for the reader; this is for the
    -- operator asking "did yesterday's briefing have anything in it".
    sections jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(sections) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, local_date)
);

ALTER TABLE viryaos_team_assignments
    DROP CONSTRAINT viryaos_team_assignments_source_kind_check;
ALTER TABLE viryaos_team_assignments
    ADD CONSTRAINT viryaos_team_assignments_source_kind_check
    CHECK (source_kind IN (
        'autopilot_action','show_task','opportunity','beacon','capture_plan',
        'daily_briefing'
    ));

-- One briefing assignment per member per briefing. The generic
-- source-identity index is WHERE source_ref IS NOT NULL and briefing
-- rows insert source_ref NULL, so it can never dedupe them — without
-- this a retried sweep could seat a second assignment (and a second
-- "initial" email) for one member on one day.
CREATE UNIQUE INDEX viryaos_team_assignments_one_briefing_per_member
    ON viryaos_team_assignments (workspace_id, source_id, assignee_member_id)
    WHERE source_kind = 'daily_briefing';

-- The briefing's day boundary is the tenant's, not UTC's. tenant_settings
-- is the per-workspace home for these values (crew_locale already lives
-- there); absence means the shipped default, which is UTC. The live
-- tenant runs on Europe/Warsaw — the same zone CROWDRELAY_TENANT_TIMEZONE
-- carries in deploy config — so existing workspaces get that value as
-- their starting point rather than an implicit midnight-UTC briefing.
INSERT INTO tenant_settings (workspace_id, key, value)
SELECT id, 'crew_timezone', 'Europe/Warsaw' FROM workspaces
ON CONFLICT (workspace_id, key) DO NOTHING;
