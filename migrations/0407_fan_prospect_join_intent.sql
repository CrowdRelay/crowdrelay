-- FAN SCOUT next-best-action vocabulary.
--
-- A warm interaction is not permission to invite somebody into the owned
-- fanbase. This stronger observation is recorded only when a source has
-- explicit evidence that the person asked how to join/follow/stay connected.

ALTER TABLE fan_prospect_observations
    DROP CONSTRAINT IF EXISTS fan_prospect_observations_observation_kind_check;

ALTER TABLE fan_prospect_observations
    ADD CONSTRAINT fan_prospect_observations_observation_kind_check
    CHECK (observation_kind IN (
        'commented_similar_band', 'collects_similar_music', 'asked_about_show',
        'asked_for_music', 'asked_to_join_or_follow', 'active_under_our_post',
        'shared_material', 'replied', 'appears_repeatedly', 'scene_participant',
        'related_to_prospects', 'active_referrer', 'content_creator',
        'attends_local_shows'
    ));
