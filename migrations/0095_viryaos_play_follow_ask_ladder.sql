-- The third play, and the first anchored on somebody rather than something.
--
-- The track-us ask reaches the fans of one show at the moment that show is the
-- thing on their mind. It therefore never reaches the fan who bought a ticket
-- last spring and has had no date near them since, which is most of the list
-- most of the time. The follow-ask ladder is for exactly those people: one ask,
-- a nudge six weeks later, a last one at four months, each with a single call
-- to action through a tracked link.
--
-- `anchor_kind` has carried only 'event' until now, and the assumption leaked:
-- every read joined `events` on `anchor_id` because there was nothing else it
-- could be. A ladder climbed by one fan over four months has no show in it, so
-- the anchor becomes a real choice here rather than a column nobody looked at.
--
-- The unique constraint on (workspace_id, play_kind, anchor_kind, anchor_id)
-- already does the work that matters: one ladder per fan, for ever. A fan who
-- ignored three asks is not asked a fourth by a restart.

ALTER TABLE viryaos_plays
    DROP CONSTRAINT IF EXISTS viryaos_plays_anchor_kind_check;
ALTER TABLE viryaos_plays
    ADD CONSTRAINT viryaos_plays_anchor_kind_check
        CHECK (anchor_kind IN ('event', 'fan'));

ALTER TABLE viryaos_plays
    DROP CONSTRAINT IF EXISTS viryaos_plays_play_kind_check;
ALTER TABLE viryaos_plays
    ADD CONSTRAINT viryaos_plays_play_kind_check
        CHECK (play_kind IN (
            'track_us_ask', 'listing_completeness_sweep', 'follow_ask_ladder'
        ));

-- One template per rung. Three sends of the same copy are three sends whose
-- separate results cannot be read, and the third message to somebody who
-- ignored two has to say something the first did not.
ALTER TABLE viryaos_play_steps
    DROP CONSTRAINT IF EXISTS viryaos_play_steps_step_kind_check;
ALTER TABLE viryaos_play_steps
    ADD CONSTRAINT viryaos_play_steps_step_kind_check
        CHECK (step_kind IN (
            'announce_ask', 'post_show_ask', 'listing_sweep',
            'follow_ask_first', 'follow_ask_second', 'follow_ask_final'
        ));

ALTER TABLE viryaos_play_learning
    DROP CONSTRAINT IF EXISTS viryaos_play_learning_play_kind_check;
ALTER TABLE viryaos_play_learning
    ADD CONSTRAINT viryaos_play_learning_play_kind_check
        CHECK (play_kind IN (
            'track_us_ask', 'listing_completeness_sweep', 'follow_ask_ladder'
        ));
