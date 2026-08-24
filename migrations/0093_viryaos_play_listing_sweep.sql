-- The second play, and the first with a step that reaches nobody.
--
-- A listing completeness sweep checks that a published upcoming show has a
-- complete free listing with a working ticket link. It is the cheapest work in
-- the whole system and the least glamorous: a show that is not listed, or
-- listed without a link, cannot be found by the people already looking for it.
--
-- It also exercises the part of the play design that had never been used. Every
-- step so far was `owned_audience` and per-recipient; this one is
-- `first_party_reversible` and runs once for its anchor. The class ceiling
-- treats it accordingly without anything here asking it to.

ALTER TABLE viryaos_plays
    DROP CONSTRAINT IF EXISTS viryaos_plays_play_kind_check;
ALTER TABLE viryaos_plays
    ADD CONSTRAINT viryaos_plays_play_kind_check
        CHECK (play_kind IN ('track_us_ask', 'listing_completeness_sweep'));

ALTER TABLE viryaos_play_steps
    DROP CONSTRAINT IF EXISTS viryaos_play_steps_step_kind_check;
ALTER TABLE viryaos_play_steps
    ADD CONSTRAINT viryaos_play_steps_step_kind_check
        CHECK (step_kind IN ('announce_ask', 'post_show_ask', 'listing_sweep'));

ALTER TABLE viryaos_play_learning
    DROP CONSTRAINT IF EXISTS viryaos_play_learning_play_kind_check;
ALTER TABLE viryaos_play_learning
    ADD CONSTRAINT viryaos_play_learning_play_kind_check
        CHECK (play_kind IN ('track_us_ask', 'listing_completeness_sweep'));
