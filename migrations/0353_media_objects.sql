-- Tenant-uploaded media (the join-ask app screenshot first): a file lands
-- as a row so blue/green containers, the worker and backups share one
-- source of bytes without a shared volume.
--
-- The upload is control-plane authenticated; the read is public because
-- Meta's crawler fetches the URL at publish time. The id is an unguessable
-- uuid — the same exposure a press-asset URL already carries. Rows are
-- append-only: there is no update route, so immutable caching on the public
-- GET is honest.
--
-- `(workspace_id, sha256)` dedupes: uploading the same file twice returns
-- the same object instead of storing a second copy.

CREATE TABLE media_objects (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    content_type text NOT NULL CHECK (content_type IN ('image/png', 'image/jpeg', 'image/webp')),
    byte_len integer NOT NULL CHECK (byte_len > 0),
    bytes bytea NOT NULL,
    name text NOT NULL DEFAULT '' CHECK (char_length(name) <= 128),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, sha256)
);
