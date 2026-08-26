-- The fifth play: release runway.
--
-- A release is the biggest organic growth moment the band gets, and the
-- runway is the sequence that makes it compound: pre-save surface live,
-- owned-audience announce, curator wave queued for approval, release-day
-- push, and a sustain ask two weeks later.
--
-- This play needs a third anchor kind — `release` — because a release is
-- not an event and not a fan. The anchor moment is `release_at` on the
-- release plan, and steps may precede it (pre-save, announce, curator wave)
-- or follow it (release-day push, sustain ask).

ALTER TABLE viryaos_plays
    DROP CONSTRAINT IF EXISTS viryaos_plays_anchor_kind_check;
ALTER TABLE viryaos_plays
    ADD CONSTRAINT viryaos_plays_anchor_kind_check
        CHECK (anchor_kind IN ('event', 'fan', 'release'));

ALTER TABLE viryaos_plays
    DROP CONSTRAINT IF EXISTS viryaos_plays_play_kind_check;
ALTER TABLE viryaos_plays
    ADD CONSTRAINT viryaos_plays_play_kind_check
        CHECK (play_kind IN (
            'track_us_ask', 'listing_completeness_sweep', 'follow_ask_ladder',
            'dormant_revival', 'release_runway'
        ));

ALTER TABLE viryaos_play_steps
    DROP CONSTRAINT IF EXISTS viryaos_play_steps_step_kind_check;
ALTER TABLE viryaos_play_steps
    ADD CONSTRAINT viryaos_play_steps_step_kind_check
        CHECK (step_kind IN (
            'announce_ask', 'post_show_ask', 'listing_sweep',
            'follow_ask_first', 'follow_ask_second', 'follow_ask_final',
            'dormant_revival_first', 'dormant_revival_final',
            'release_presave_live', 'release_audience_announce',
            'release_curator_wave', 'release_day_push', 'release_sustain_ask'
        ));

ALTER TABLE viryaos_play_learning
    DROP CONSTRAINT IF EXISTS viryaos_play_learning_play_kind_check;
ALTER TABLE viryaos_play_learning
    ADD CONSTRAINT viryaos_play_learning_play_kind_check
        CHECK (play_kind IN (
            'track_us_ask', 'listing_completeness_sweep', 'follow_ask_ladder',
            'dormant_revival', 'release_runway'
        ));
