-- R+3 release outcome report (1R.7): the honest "who arrived because of this
-- release" read. Two pieces land here:
--
-- 1. `campaigns.release_plan_id` binds the acquisition-attribution unit to the
--    release plan it exists for. The tracked link (`release-{slug}`) already
--    upserts per milestone; binding it to a campaign carrying this column is
--    what makes clicks and signups attributable to the release instead of to
--    ambient growth. One campaign per release — created lazily by
--    `ensure_release_tracked_link` once a listen_url exists.
--
-- ON DELETE SET NULL is column-scoped so a deleted plan detaches the campaign
-- rather than the workspace row.

ALTER TABLE campaigns
    ADD COLUMN release_plan_id uuid;

ALTER TABLE campaigns
    ADD CONSTRAINT campaigns_release_plan_fk
        FOREIGN KEY (workspace_id, release_plan_id)
        REFERENCES viryaos_release_plans (workspace_id, id)
        ON DELETE SET NULL (release_plan_id);

CREATE INDEX campaigns_release_plan_idx
    ON campaigns (workspace_id, release_plan_id)
    WHERE release_plan_id IS NOT NULL;
