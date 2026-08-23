-- The brief the operator never got.
--
-- Every read model a brief needs already existed — chief-of-staff, the control
-- overview, growth-metric coverage — and nothing ever sent one. That is not a
-- cosmetic gap. In production the agent sat with its envelope disabled and a
-- dozen decisions awaiting approval, and from outside that is indistinguishable
-- from a quiet week. An operator who cannot see what the agent did, or what it
-- is stuck on, eventually switches it off; an operator who is told daily that
-- nothing happened stops reading first.
--
-- So this table is not a message log. It is the durable answer to "when did we
-- last say something, and what did we lead with", which is what makes the
-- once-a-day rule enforceable across restarts and what makes the brief
-- auditable afterwards.
--
-- Deliberately not an outbox lookup: outbox rows are pruned by retention, and
-- an idempotency guarantee that expires with a retention window is not one.

CREATE TABLE viryaos_operator_briefs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The single fact the brief led with. A closed set, because a headline the
    -- rule cannot produce is a headline nobody can act on.
    headline text NOT NULL CHECK (headline IN (
        'worked', 'blind', 'failing', 'awaiting_approval',
        'work_parked', 'disabled_with_work_waiting', 'approval_stale'
    )),
    -- The evidence the headline was chosen from, so a brief can be re-read
    -- later against what was true at the time rather than what is true now.
    snapshot jsonb NOT NULL CHECK (jsonb_typeof(snapshot) = 'object'),
    sent_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);

-- The only query the rule runs: when did we last brief this workspace.
CREATE INDEX viryaos_operator_briefs_recent_idx
    ON viryaos_operator_briefs (workspace_id, sent_at DESC);
