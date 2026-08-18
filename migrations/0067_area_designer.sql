-- Tenant AREA Designer: draft/publish lifecycle, optimistic revisions and entitlement.
-- Exact claim coordinates remain stored only in the tenant CrowdRelay database.

ALTER TABLE area_drops
    ADD COLUMN IF NOT EXISTS revision bigint NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS sort_order integer NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS published_at timestamptz,
    ADD COLUMN IF NOT EXISTS archived_at timestamptz;

-- Existing VIRYA rows predate the Designer but are already canonical runtime data.
UPDATE area_drops
SET published_at = COALESCE(published_at, created_at)
WHERE published_at IS NULL;

ALTER TABLE area_drops DROP CONSTRAINT IF EXISTS area_drops_workspace_id_city_id_key;
ALTER TABLE area_drops DROP CONSTRAINT IF EXISTS area_drops_workspace_id_number_key;
CREATE UNIQUE INDEX IF NOT EXISTS area_drops_workspace_current_city_uidx
    ON area_drops (workspace_id, city_id)
    WHERE archived_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS area_drops_workspace_current_number_uidx
    ON area_drops (workspace_id, number)
    WHERE archived_at IS NULL;
CREATE INDEX IF NOT EXISTS area_drops_workspace_designer_idx
    ON area_drops (workspace_id, archived_at, sort_order, number, id);

CREATE TABLE IF NOT EXISTS area_drop_drafts (
    workspace_id uuid NOT NULL,
    drop_id text NOT NULL,
    base_revision bigint NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, drop_id),
    FOREIGN KEY (workspace_id)
        REFERENCES workspaces(id)
        ON DELETE CASCADE,
    CHECK (drop_id ~ '^[a-z]{3}-[0-9]{3}$'),
    CHECK (base_revision >= 0),
    CHECK (jsonb_typeof(payload) = 'object')
);

CREATE TABLE IF NOT EXISTS area_workspace_settings (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    enabled boolean NOT NULL DEFAULT false,
    default_radius_meters integer NOT NULL DEFAULT 100,
    default_max_claims integer NOT NULL DEFAULT 25,
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (default_radius_meters BETWEEN 25 AND 500),
    CHECK (default_max_claims BETWEEN 1 AND 500)
);

INSERT INTO area_workspace_settings (workspace_id, enabled)
SELECT
    workspace.id,
    workspace.slug = 'virya'
        OR EXISTS (
            SELECT 1
            FROM area_drops AS existing_drop
            WHERE existing_drop.workspace_id = workspace.id
        )
FROM workspaces AS workspace
ON CONFLICT (workspace_id) DO NOTHING;
