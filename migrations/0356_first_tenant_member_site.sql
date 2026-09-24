-- The first tenant's member-site URL, as the explicit setting it always was
-- in effect.
--
-- `member_site_base_url` fell back to a shipped default of
-- 'https://virya.music' — the first tenant's own site — for every workspace
-- without a row. For any other tenant that default was another band's
-- signup page: its join-ask posts, beacon invitations and release mails
-- would have sent its fans and contacts there. The default is now empty, and
-- an empty site URL holds the join-ask (`NoSiteUrl`) and refuses the
-- invitations instead of sending a link that is not the tenant's.
--
-- The `virya` workspace is the tenant the default was written for. It gets
-- the value it has been reading all along, so nothing it sends changes. A
-- row it already has is kept as is: this never overwrites a setting.

INSERT INTO tenant_settings (workspace_id, key, value)
SELECT workspace.id, 'member_site_base_url', 'https://virya.music'
FROM workspaces AS workspace
WHERE workspace.slug = 'virya'
ON CONFLICT (workspace_id, key) DO NOTHING;
