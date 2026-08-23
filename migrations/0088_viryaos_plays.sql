-- Plays: the agent's unit of work, and the first thing here that remembers.
--
-- Every context before this one answers a question per cycle and forgets. That
-- is the right shape for a detector and the wrong shape for a campaign, because
-- the growth work that matters is not one message — it is a message when a date
-- near a fan is announced, a second one the morning after they enjoyed it, and
-- a reading of whether either moved anything. Without durable state step two
-- either never happens or happens every cycle for ever.
--
-- Three tables, and each exists because merging it into another loses something:
--
-- * `viryaos_plays` is the campaign and its anchor. One live play per
--   (kind, anchor) so a show cannot accumulate duplicate campaigns across
--   restarts, evaluation races or a replayed decision.
-- * `viryaos_play_steps` is the state machine. A step is settled exactly once,
--   and `settled_at`/`skip_reason` make an omission a recorded fact rather than
--   an absence somebody has to notice. This is what lets a gated step be skipped
--   instead of hanging, and lets the step behind it run anyway.
-- * `viryaos_play_step_recipients` is who a step already reached. Without it a
--   resumed play re-sends to everyone it already messaged, which is the one
--   failure mode an owned-audience campaign must never have: a message cannot
--   be unsent.
--
-- No scheduler and no timer. `due_at` and `expires_at` are derived from the
-- anchor when the play starts, and the existing autopilot cycle finds work by
-- asking what is due now — so a deploy cannot lose a pending step.
--
-- The context is provisioned disabled and at 'observe', like every context
-- before it. A play that starts running because a migration landed is a play
-- nobody chose to run.

CREATE TABLE viryaos_plays (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    play_kind text NOT NULL CHECK (play_kind IN ('track_us_ask')),
    -- The fact the schedule hangs off. Kept as a loose reference on purpose,
    -- matching the growth-metric series: a play is a record of what the agent
    -- did and must not block deletion of the business row it was about.
    anchor_kind text NOT NULL CHECK (anchor_kind IN ('event')),
    anchor_id uuid NOT NULL,
    anchor_at timestamptz NOT NULL,
    state text NOT NULL DEFAULT 'running'
        CHECK (state IN ('running', 'completed')),
    -- What the play claimed would happen, frozen at start. Read back against
    -- the measurement rather than reconstructed from whatever the code says
    -- today, because the code will change and the claim should not.
    hypothesis text NOT NULL CHECK (btrim(hypothesis) <> ''),
    -- The series the play is judged against. Correlational by construction, and
    -- named here so a later measurement cannot quietly pick a friendlier metric.
    success_metric_platform text NOT NULL,
    success_metric_key text NOT NULL,
    started_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    -- One play per kind per anchor, for ever. Not "one running play": allowing
    -- a second after the first completes would let a restart re-run a campaign
    -- against fans who already had it.
    UNIQUE (workspace_id, play_kind, anchor_kind, anchor_id),
    CHECK ((state = 'completed') = (completed_at IS NOT NULL))
);

CREATE TRIGGER viryaos_plays_set_updated_at
BEFORE UPDATE ON viryaos_plays
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

-- The cycle asks "which running plays have work" every few minutes.
CREATE INDEX viryaos_plays_running_idx
    ON viryaos_plays (workspace_id, anchor_at)
    WHERE state = 'running';

CREATE TABLE viryaos_play_steps (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL,
    play_id uuid NOT NULL,
    step_index integer NOT NULL CHECK (step_index >= 0 AND step_index < 32),
    step_kind text NOT NULL CHECK (step_kind IN ('announce_ask', 'post_show_ask')),
    -- The step's own class. The class ceiling already downgrades an outward
    -- step to require_approval on its own, so a play needs no second permission
    -- system and cannot smuggle a curator email through an owned-audience play.
    action_class text NOT NULL CHECK (action_class IN (
        'first_party_reversible', 'owned_audience', 'third_party', 'paid'
    )),
    due_at timestamptz NOT NULL,
    -- Past this the step is settled as skipped. A post-show thank-you sent
    -- three weeks late is a different, worse message than one not sent.
    expires_at timestamptz NOT NULL CHECK (expires_at > due_at),
    settled_at timestamptz,
    -- Present only on a step that was settled without being delivered. This is
    -- what makes an omission auditable: `window_closed` on a third-party step
    -- means nobody approved it in time, and that is a fact about the operator's
    -- queue rather than about the play.
    skip_reason text CHECK (skip_reason IS NULL OR skip_reason IN (
        'window_closed', 'no_eligible_recipients', 'anchor_withdrawn'
    )),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_play_steps_play_fk
        FOREIGN KEY (workspace_id, play_id)
        REFERENCES viryaos_plays (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, play_id, step_index),
    -- A skip reason on an unsettled step would describe a step that is still
    -- running, and a settled step is never revisited.
    CHECK (skip_reason IS NULL OR settled_at IS NOT NULL)
);

CREATE TRIGGER viryaos_play_steps_set_updated_at
BEFORE UPDATE ON viryaos_play_steps
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

-- The only step query that runs every cycle: the earliest unsettled step of a
-- running play. Partial, because settled steps are never looked at again.
CREATE INDEX viryaos_play_steps_open_idx
    ON viryaos_play_steps (workspace_id, play_id, step_index)
    WHERE settled_at IS NULL;

CREATE TABLE viryaos_play_step_recipients (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL,
    step_id uuid NOT NULL,
    fan_id uuid NOT NULL REFERENCES fans(id) ON DELETE CASCADE,
    -- The action that carried it, so a delivery can be traced from the fan back
    -- to the decision that chose them.
    action_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_play_step_recipients_step_fk
        FOREIGN KEY (workspace_id, step_id)
        REFERENCES viryaos_play_steps (workspace_id, id)
        ON DELETE CASCADE,
    -- The row that makes a resumed play safe. A fan is reached by a step once,
    -- and the database is what enforces it rather than a query the evaluator
    -- has to remember to write.
    UNIQUE (workspace_id, step_id, fan_id)
);

CREATE INDEX viryaos_play_step_recipients_step_idx
    ON viryaos_play_step_recipients (workspace_id, step_id);

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt','outreach_supply','plays'
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
        'growth_metrics','growth_debt','outreach_supply','plays'
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
        'growth_metrics','growth_debt','outreach_supply','plays'
    ));

ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_subject_kind_check;

-- A play step is emitted per recipient, so the quota is the number of sends the
-- agent may make in a day across every running play. Sized against the envelope's
-- weekly owned-audience budget rather than against the step ceiling: whichever
-- is tighter should bind, and the envelope is the one an operator reasons about.
--
-- Both halves are needed. The backfill covers workspaces that already exist; the
-- trigger covers the ones created later. A context added to only one of them
-- works perfectly until the next workspace is created.
INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT workspace.id, 'plays', 40
FROM workspaces workspace
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
        (NEW.id, 'growth_debt', 10),
        (NEW.id, 'growth_metrics', 12),
        (NEW.id, 'outreach_supply', 2),
        (NEW.id, 'plays', 40)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;
