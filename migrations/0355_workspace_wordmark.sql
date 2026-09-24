-- The name a workspace's fan-facing messages carry.
--
-- Push titles, play-step pushes and similar copy hardcoded "VIRYA" — the
-- first tenant's wordmark — so every other tenant's fans would have been
-- told "VIRYA — new show" about somebody else's show, and every act on a
-- roster would have spoken as one band. This is the one place the name is
-- resolved, so SQL-built copy and Rust-built copy cannot disagree.
--
-- Per workspace, not per process: a roster runs several acts' workspaces in
-- one runtime, and each act speaks as itself.
--
-- 1. An explicit `brand_wordmark` tenant setting, when the operator set one.
-- 2. Otherwise, for the `virya` workspace, 'VIRYA' — the wordmark every
--    message it has ever sent carried, so its fans see no change.
-- 3. Otherwise the workspace's own name — the act name roster views
--    already show. `workspaces_name_check` keeps it non-blank, so there is
--    no emptier fallback to reach for.

CREATE OR REPLACE FUNCTION crowdrelay_workspace_wordmark(p_workspace_id uuid)
RETURNS text
LANGUAGE sql
STABLE
AS $$
    SELECT COALESCE(
        (
            SELECT NULLIF(btrim(setting.value), '')
            FROM tenant_settings AS setting
            WHERE setting.workspace_id = p_workspace_id
              AND setting.key = 'brand_wordmark'
        ),
        (
            SELECT CASE WHEN workspace.slug = 'virya' THEN 'VIRYA' ELSE workspace.name END
            FROM workspaces AS workspace
            WHERE workspace.id = p_workspace_id
        )
    )
$$;

-- The prefix a workspace's ticket, pass and draw references carry.
--
-- Minted as 'VIRYA-…' for every tenant, so a fan of any other band held a
-- ticket stamped with the first tenant's name. References need only be
-- globally unique (the random part guarantees that), so the prefix is free
-- to be the workspace's own: 'VIRYA' for the first tenant, unchanged; for
-- anyone else, its slug with the separators dropped, capped at twelve
-- characters — a reference is read aloud at a door and typed into a support
-- mail. `workspaces_slug_check` (^[a-z0-9][a-z0-9_-]*$) guarantees at least
-- one letter or digit survives, and that nothing outside ASCII is there.

CREATE OR REPLACE FUNCTION crowdrelay_workspace_reference_prefix(p_workspace_id uuid)
RETURNS text
LANGUAGE sql
STABLE
AS $$
    SELECT CASE
               WHEN workspace.slug = 'virya' THEN 'VIRYA'
               ELSE upper(left(regexp_replace(workspace.slug, '[^a-z0-9]', '', 'g'), 12))
           END
    FROM workspaces AS workspace
    WHERE workspace.id = p_workspace_id
$$;
