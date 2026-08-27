-- Agent outcome handoff table + fan segment + outreach target staging.
--
-- The agents service (crowdrelay-agents, TypeScript) is the ONLY writer of
-- agent_outcomes. The Rust AgentOutcomeWorker is the only reader/mapper.
-- Rows are keyed by (workspace_id, idempotency_key) so worker retries and
-- task re-runs can never double-create autopilot decisions.
--
-- agent_fan_segments: LLM-proposed audience segments, single-writer = Rust worker.
-- agent_outreach_targets: staging table for LLM-proposed contacts before they
--   are promoted into viryaos_outreach_targets (which requires verified email).

CREATE TABLE agent_outcomes (
  id                      UUID PRIMARY KEY,
  workspace_id            UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  task_id                 UUID NOT NULL,
  result_id               UUID NOT NULL,
  kind                    TEXT NOT NULL CHECK (kind IN (
        'press_pitch','social_post','audience_segments','outreach_targets',
        'campaign_insight','release_plan_note','generic_insight')),
  schema_version          INT  NOT NULL DEFAULT 1,
  payload                 JSONB NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
  confidence_basis_points INT  NOT NULL CHECK (confidence_basis_points BETWEEN 0 AND 10000),
  status                  TEXT NOT NULL DEFAULT 'pending'
                          CHECK (status IN ('pending','processing','processed','rejected')),
  rejection_reason        TEXT,
  idempotency_key         TEXT NOT NULL,
  processed_decision_id   UUID,
  processed_action_id     UUID,
  processed_at            TIMESTAMPTZ,
  created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (workspace_id, idempotency_key)
);

CREATE INDEX agent_outcomes_pending_idx
  ON agent_outcomes (created_at) WHERE status = 'pending';
CREATE INDEX agent_outcomes_ws_idx
  ON agent_outcomes (workspace_id, created_at DESC);

CREATE TABLE agent_fan_segments (
  id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  name            TEXT NOT NULL,
  description     TEXT NOT NULL DEFAULT '',
  size_estimate   INT,
  criteria        JSONB NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(criteria)='object'),
  source_task_id  UUID,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (workspace_id, name)
);

CREATE INDEX agent_fan_segments_workspace_idx
  ON agent_fan_segments (workspace_id, created_at DESC);

CREATE TABLE agent_outreach_targets (
  id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  target_kind     TEXT NOT NULL CHECK (target_kind IN (
    'press', 'radio', 'playlist', 'media_patronage', 'endorsement', 'creator'
  )),
  display_name    TEXT NOT NULL CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 200),
  contact_email   TEXT CHECK (contact_email IS NULL OR char_length(contact_email) <= 320),
  contact_domain  TEXT CHECK (contact_domain IS NULL OR char_length(contact_domain) <= 200),
  why_fit         TEXT NOT NULL DEFAULT '',
  evidence        JSONB NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(evidence) = 'array'),
  verified        BOOLEAN NOT NULL DEFAULT false,
  accepts_outreach BOOLEAN NOT NULL DEFAULT false,
  do_not_contact  BOOLEAN NOT NULL DEFAULT false,
  status          TEXT NOT NULL DEFAULT 'proposed'
                  CHECK (status IN ('proposed', 'promoted', 'discarded')),
  source_task_id  UUID,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX agent_outreach_targets_workspace_idx
  ON agent_outreach_targets (workspace_id, status, created_at DESC);
