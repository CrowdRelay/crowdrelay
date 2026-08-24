-- The fourth play: write once more to the fans who stopped turning up.
--
-- A quiet fan is not a lost one. The list already holds people who bought a
-- ticket two years ago and have heard nothing since, and reaching them costs
-- the band nothing — it is the cheapest audience left and the only one the
-- agent currently does nothing with.
--
-- Two messages and then it stops. Somebody who ignored the band for a year and
-- then ignored two reminders has answered, and a third would be the campaign
-- talking to itself.
--
-- Its success metric is `signal/activated_fans_30d`, which is the thing being
-- attempted rather than a proxy for it: a revived fan is a fan who became
-- active again, and that number is first-party and observed rather than read
-- off somebody else's platform.

ALTER TABLE viryaos_plays
    DROP CONSTRAINT IF EXISTS viryaos_plays_play_kind_check;
ALTER TABLE viryaos_plays
    ADD CONSTRAINT viryaos_plays_play_kind_check
        CHECK (play_kind IN (
            'track_us_ask', 'listing_completeness_sweep', 'follow_ask_ladder',
            'dormant_revival'
        ));

ALTER TABLE viryaos_play_steps
    DROP CONSTRAINT IF EXISTS viryaos_play_steps_step_kind_check;
ALTER TABLE viryaos_play_steps
    ADD CONSTRAINT viryaos_play_steps_step_kind_check
        CHECK (step_kind IN (
            'announce_ask', 'post_show_ask', 'listing_sweep',
            'follow_ask_first', 'follow_ask_second', 'follow_ask_final',
            'dormant_revival_first', 'dormant_revival_final'
        ));

ALTER TABLE viryaos_play_learning
    DROP CONSTRAINT IF EXISTS viryaos_play_learning_play_kind_check;
ALTER TABLE viryaos_play_learning
    ADD CONSTRAINT viryaos_play_learning_play_kind_check
        CHECK (play_kind IN (
            'track_us_ask', 'listing_completeness_sweep', 'follow_ask_ladder',
            'dormant_revival'
        ));

-- The revival anchor query asks, per candidate fan, whether any play step has
-- reached them lately. Without this that is a scan of every recipient row in
-- the workspace per fan, every cycle.
CREATE INDEX IF NOT EXISTS viryaos_play_step_recipients_fan_idx
    ON viryaos_play_step_recipients (workspace_id, fan_id, created_at DESC);
