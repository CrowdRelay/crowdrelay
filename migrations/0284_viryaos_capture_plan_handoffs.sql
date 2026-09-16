-- Capture-plan handoffs: the production day gets a real shot list, routed.
--
-- 0280 created the tables; this migration wires the lifecycle. Published
-- shows project into viryaos_production_events so every gig is also a
-- production day. The team-handoff sweep issues a capture plan at T-1 —
-- a shot list built from the formats the open suggestions and the active
-- arc still need — and routes it to the member who holds the camera
-- through the same assignment + reminder machinery every human handoff
-- uses. After the day, the sweep counts the content sources the day
-- actually produced and settles the plan done or abandoned.
--
-- source_kind 'capture_plan': the assignment points at the plan row, the
-- way 'show_task' points at (event, item_key). The bare 'capture_plan'
-- checklist task it replaces asked "does anyone have a plan"; the plan
-- row now answers that question itself.

ALTER TABLE viryaos_team_assignments
    DROP CONSTRAINT viryaos_team_assignments_source_kind_check;
ALTER TABLE viryaos_team_assignments
    ADD CONSTRAINT viryaos_team_assignments_source_kind_check
    CHECK (source_kind IN (
        'autopilot_action','show_task','opportunity','beacon','capture_plan'
    ));

-- NULLS NOT DISTINCT made (workspace_id, NULL) unique: show-task, beacon,
-- opportunity and capture-plan assignments all insert action_id NULL, so
-- the constraint capped non-action handoffs at one row per workspace —
-- forever, open or closed, with ON CONFLICT DO NOTHING hiding every
-- refusal. NULLS DISTINCT keeps one-assignment-per-action while letting
-- any number of source-keyed rows coexist, which is what
-- viryaos_team_assignments_source_identity_uidx was built to dedupe.
ALTER TABLE viryaos_team_assignments
    DROP CONSTRAINT viryaos_team_assignments_workspace_id_action_id_key;
ALTER TABLE viryaos_team_assignments
    ADD CONSTRAINT viryaos_team_assignments_workspace_id_action_id_key
    UNIQUE (workspace_id, action_id);

-- One open plan per production day. Two sweeps racing the same workspace
-- must not issue two shot lists for one day, and a settled plan must not
-- block a re-issue if the day is rescheduled.
CREATE UNIQUE INDEX viryaos_capture_plans_one_open_per_event
    ON viryaos_capture_plans (production_event_id)
    WHERE status IN ('draft','issued');

-- One projected show per gig. The projection is idempotent on event_id;
-- without the index two concurrent sweeps could both pass NOT EXISTS and
-- insert twin production days for the same show. Other kinds may share
-- an event_id legitimately (a photoshoot pinned to the gig).
CREATE UNIQUE INDEX viryaos_production_events_one_show_per_gig
    ON viryaos_production_events (event_id)
    WHERE event_id IS NOT NULL AND kind = 'show';

-- One open assignment per capture plan. The generic source-identity
-- index is WHERE source_ref IS NOT NULL and capture-plan rows insert
-- source_ref NULL, so it can never dedupe them — without this a retried
-- route could seat a second assignment (and a second "initial" email)
-- for one plan.
CREATE UNIQUE INDEX viryaos_team_assignments_one_open_per_capture_plan
    ON viryaos_team_assignments (workspace_id, source_id)
    WHERE source_kind = 'capture_plan' AND status = 'open';
