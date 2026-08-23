-- How much the growth agent may do, on top of what kinds of thing it may do.
--
-- Migration 0075 set the ceiling per action class. This sets the volume: a
-- weekly budget for each outward class, a per-subject cooldown, a blast radius,
-- a rehearsal mode and a kill switch. Every one of them exists because its
-- absence has a specific failure mode -- an unbounded send, one fan hearing
-- from four plays in a morning, a wrong segment costing the whole list, no way
-- to stop the agent without a deploy.
--
-- No new ledger. Outward touches are already durable rows in
-- `viryaos_autopilot_actions`; this counts them. A second ledger would be one
-- more thing that can disagree with the actions it claims to describe.

CREATE TABLE viryaos_growth_envelope (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The kill switch, off until somebody chooses otherwise. An agent that
    -- starts acting the moment its migration lands is one nobody switched on.
    agent_enabled boolean NOT NULL DEFAULT false,
    -- Rehearsal: produce every decision with its evidence and execute nothing.
    -- Separate from the switch on purpose, so turning the agent on is not also
    -- the moment it first sends something real.
    dry_run boolean NOT NULL DEFAULT true,
    weekly_owned_audience_touches integer NOT NULL DEFAULT 200
        CHECK (weekly_owned_audience_touches BETWEEN 0 AND 100000),
    -- Low even once the ceiling widens: these are finite relationships, and
    -- the band gets one first approach to each of them.
    weekly_third_party_touches integer NOT NULL DEFAULT 10
        CHECK (weekly_third_party_touches BETWEEN 0 AND 1000),
    subject_cooldown_hours integer NOT NULL DEFAULT 168
        CHECK (subject_cooldown_hours BETWEEN 0 AND 8760),
    max_recipients_per_step integer NOT NULL DEFAULT 250
        CHECK (max_recipients_per_step BETWEEN 1 AND 100000),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER viryaos_growth_envelope_set_updated_at
BEFORE UPDATE ON viryaos_growth_envelope
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

INSERT INTO viryaos_growth_envelope (workspace_id)
SELECT id FROM workspaces
ON CONFLICT (workspace_id) DO NOTHING;

CREATE OR REPLACE FUNCTION viryaos_provision_growth_envelope()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_growth_envelope (workspace_id)
    VALUES (NEW.id)
    ON CONFLICT (workspace_id) DO NOTHING;
    RETURN NEW;
END;
$$;

CREATE TRIGGER workspaces_provision_growth_envelope
AFTER INSERT ON workspaces
FOR EACH ROW
EXECUTE FUNCTION viryaos_provision_growth_envelope();

-- What an action cost and who it reached, recorded when it was created rather
-- than derived at read time.
--
-- Deriving it later would mean reimplementing the Rust classification in SQL,
-- and the two would drift the first time a show-growth lever was reclassified.
-- Recording it also makes the column mean the right thing: the class the action
-- was *authorised under*, which is what an audit needs, not the class the
-- current build would assign it.
--
-- Nullable on purpose. NULL means the action predates the autonomy envelope,
-- and those rows are deliberately not counted against the agent's budget --
-- work done before the agent existed was not the agent's.
ALTER TABLE viryaos_autopilot_actions
    ADD COLUMN action_class text CHECK (action_class IS NULL OR action_class IN (
        'first_party_reversible', 'owned_audience', 'third_party', 'paid'
    ));

-- Budget counting reads a rolling seven days per class; the cooldown reads the
-- newest outward action per subject. Both are covered here.
CREATE INDEX viryaos_autopilot_actions_outward_idx
    ON viryaos_autopilot_actions (workspace_id, action_class, created_at DESC)
    WHERE action_class IN ('owned_audience', 'third_party');

CREATE INDEX viryaos_autopilot_actions_subject_outward_idx
    ON viryaos_autopilot_actions (workspace_id, subject_id, created_at DESC)
    WHERE action_class IN ('owned_audience', 'third_party');
