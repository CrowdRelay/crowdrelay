-- Release outcome records (1R.12) + the R+30 catalogue-rotation rung (1R.8)
-- + the R-14 making-of handoff source kind (1R.4).
--
-- `viryaos_release_outcomes` is the durable, queryable half of the honest
-- wave reports: the R+3 and R+14 payloads ride the outbox to the band, but
-- learning needs a row it can join on — tier × timing × verdict × evidence
-- — from the first release onward, before the sample is large enough to
-- read. One row per (release, report kind); a re-fired milestone upserts the
-- same row rather than doubling the record.

CREATE TABLE viryaos_release_outcomes (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    release_id uuid NOT NULL,
    report_kind text NOT NULL CHECK (report_kind IN ('release_r3', 'release_r14')),
    tier text NOT NULL CHECK (tier IN ('single','track','filler')),
    release_at timestamptz NOT NULL,
    generated_at timestamptz NOT NULL DEFAULT now(),
    window_days integer NOT NULL CHECK (window_days > 0),
    verdict text NOT NULL CHECK (
        verdict IN ('above_trend','within_noise','insufficient_evidence')
    ),
    action_id uuid,
    payload jsonb NOT NULL,
    PRIMARY KEY (workspace_id, release_id, report_kind),
    CONSTRAINT viryaos_release_outcomes_release_fk
        FOREIGN KEY (workspace_id, release_id)
        REFERENCES viryaos_release_plans (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX viryaos_release_outcomes_tier_idx
    ON viryaos_release_outcomes (workspace_id, tier, release_at);

-- R+30 catalogue rotation (§4i-4): the last rung of the ladder. The send goes
-- only to fans no earlier release phase reached — rotating by what a specific
-- fan has not seen, never by oldest — and it rides the same milestone,
-- collision-hold, and campaign machinery as every other wave, so it spends
-- from the same attention budget rather than adding to it.
ALTER TABLE viryaos_release_milestones
    DROP CONSTRAINT IF EXISTS viryaos_release_milestones_milestone_check;
ALTER TABLE viryaos_release_milestones
    ADD CONSTRAINT viryaos_release_milestones_milestone_check
        CHECK (milestone IN (
            'seed_calendar','editorial_pitch','announcement','start_press','fan_warmup',
            'countdown','release_day','sustain','wrap','catalogue_rotation'
        ));

-- The R-14 making-of handoff (§4b-3): a release assignment asking for the
-- making-of material that already exists to be filed and labelled, routed by
-- the same team machinery as show tasks and capture plans.
ALTER TABLE viryaos_team_assignments
    DROP CONSTRAINT viryaos_team_assignments_source_kind_check;
ALTER TABLE viryaos_team_assignments
    ADD CONSTRAINT viryaos_team_assignments_source_kind_check
    CHECK (source_kind IN (
        'autopilot_action','show_task','opportunity','beacon','capture_plan',
        'daily_briefing','release_making_of'
    ));
