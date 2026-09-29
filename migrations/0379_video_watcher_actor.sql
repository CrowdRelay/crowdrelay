-- Let the video source sync sign the release plans it creates.
--
-- The YouTube upload sync asks the autopilot repository to open a release
-- plan when a fresh video appears, and the plan's audit row was recorded as
-- `admin_api_key` because that was the only caller. An operator reading the
-- ledger cannot tell a plan the watcher proposed from one a person entered.
-- Widening the CHECK adds `video-watcher`; no existing row moves.

ALTER TABLE operator_actions
    DROP CONSTRAINT IF EXISTS operator_actions_actor_type_check;
ALTER TABLE operator_actions
    ADD CONSTRAINT operator_actions_actor_type_check
        CHECK (actor_type IN ('admin_api_key', 'executor', 'video-watcher'));
