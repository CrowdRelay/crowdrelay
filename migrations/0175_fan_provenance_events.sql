-- Fan provenance events — append-only exposure/interaction/conversion/durability ledger.
--
-- PROVENANCE ≠ CAUSALITY
--
-- Fan provenance events establish exposure/attribution evidence. They do
-- NOT automatically establish causal treatment effect. The semantic layers
-- are:
--   EXPOSURE → ATTRIBUTION → CAUSAL ESTIMATE
-- kept separate at all times.
--
-- Event semantics (event_kind):
--   exposure    — fan was exposed to an action (post seen, email received)
--   interaction — fan engaged (click, reply, share)
--   conversion  — fan signed up / became a fan
--   durability  — fan still active after 30 days
--
-- This lets the ledger reconstruct:
--   community exposure → interaction → conversion → durable fan
-- rather than reducing to a single `origin_community` field on `fans`.
--
-- fan_id is nullable because exposure events may be anonymous (before
-- the fan is known). Once the fan converts, the fan_id is linked.

CREATE TABLE IF NOT EXISTS fan_provenance_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid REFERENCES fans(id) ON DELETE SET NULL,
    event_kind text NOT NULL CHECK (event_kind IN ('exposure', 'interaction', 'conversion', 'durability')),
    channel text NOT NULL CHECK (btrim(channel) <> ''),
    source_target text,
    community text,
    campaign_id uuid,
    action_id uuid,
    attribution_method text NOT NULL DEFAULT 'unknown',
    attribution_confidence double precision NOT NULL DEFAULT 0.0,
    occurred_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_fan_provenance_workspace_fan
    ON fan_provenance_events (workspace_id, fan_id, occurred_at);

CREATE INDEX IF NOT EXISTS idx_fan_provenance_community
    ON fan_provenance_events (workspace_id, community, occurred_at);

CREATE INDEX IF NOT EXISTS idx_fan_provenance_action
    ON fan_provenance_events (workspace_id, action_id)
    WHERE action_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_fan_provenance_event_kind
    ON fan_provenance_events (workspace_id, event_kind, occurred_at);
