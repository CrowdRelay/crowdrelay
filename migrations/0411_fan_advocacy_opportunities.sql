-- FAN SCOUT slice 2B: one durable first-advocacy opportunity per person.
--
-- A light referral ask is not a Latarnik role. It is a planning fact saying
-- the canonical advocacy evaluator found enough first-party evidence to ask
-- this person to carry one tracked referral to one relevant friend.
--
-- The row is person-keyed so a fan merge cannot duplicate or erase the one-ask
-- budget. Nothing here sends a message and nothing grants authority.

CREATE TABLE fan_advocacy_opportunities (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    person_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind = 'personal_referral'),
    status text NOT NULL DEFAULT 'ready'
        CHECK (status IN ('ready', 'offered', 'completed', 'cancelled')),
    source text NOT NULL CHECK (btrim(source) <> ''),
    evidence jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(evidence) = 'object'),
    ready_at timestamptz NOT NULL,
    offered_at timestamptz,
    completed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, person_id, kind),
    CONSTRAINT fan_advocacy_opportunities_person_fk
        FOREIGN KEY (workspace_id, person_id)
        REFERENCES persons (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT fan_advocacy_opportunities_offered_has_time
        CHECK (status <> 'offered' OR offered_at IS NOT NULL),
    CONSTRAINT fan_advocacy_opportunities_completed_has_time
        CHECK (status <> 'completed' OR completed_at IS NOT NULL)
);

CREATE INDEX fan_advocacy_opportunities_ready_idx
    ON fan_advocacy_opportunities (workspace_id, status, ready_at, id)
    WHERE status = 'ready';
