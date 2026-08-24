-- The Spotify editorial pitch: the one piece of release work the agent must
-- never claim to have done.
--
-- It is a single form per release inside Spotify for Artists and there is no
-- API for it. What the agent can do is everything around the form: detect the
-- release, work out the deadline — the pitch has to be in before the
-- distributor delivers the track — assemble the text and the evidence, put it
-- in front of a human, and refuse to let the deadline slip quietly.
--
-- Two columns rather than a milestone row, because this one repeats. A
-- milestone is delivered once and recorded once; an unfinished pitch is chased
-- until somebody says it is finished, and only a human can say that.

ALTER TABLE viryaos_release_plans
    -- Set by an operator saying they submitted the form. Nothing the agent can
    -- read would tell it, and inferring it from silence is how a release goes
    -- out with no pitch and a green dashboard.
    ADD COLUMN editorial_pitch_completed_at timestamptz,
    -- The last chase, so a reminder is a reminder rather than a stream.
    ADD COLUMN editorial_pitch_escalated_at timestamptz;

ALTER TABLE viryaos_release_milestones
    DROP CONSTRAINT IF EXISTS viryaos_release_milestones_milestone_check;
ALTER TABLE viryaos_release_milestones
    ADD CONSTRAINT viryaos_release_milestones_milestone_check
        CHECK (milestone IN (
            'seed_calendar','editorial_pitch','announcement','start_press','fan_warmup',
            'countdown','release_day','sustain','wrap'
        ));
