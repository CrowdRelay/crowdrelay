-- Auditable AI-assisted event copy. Provider facts stay immutable in
-- source_description while the public description may be enriched or manually
-- curated. Every generation request is content-addressed so retries and source
-- changes are safe.

ALTER TABLE events
    ADD COLUMN source_description text,
    ADD COLUMN description_origin text NOT NULL DEFAULT 'manual' CHECK (
        description_origin IN ('manual', 'provider', 'ai')
    ),
    ADD COLUMN description_source_hash bytea CHECK (
        description_source_hash IS NULL OR octet_length(description_source_hash) = 32
    ),
    ADD COLUMN description_language char(2) NOT NULL DEFAULT 'pl' CHECK (
        description_language ~ '^[a-z]{2}$'
    );

UPDATE events
SET source_description = description,
    description_origin = CASE
        WHEN source_id IS NULL THEN 'manual'
        ELSE 'provider'
    END
WHERE source_description IS NULL;

ALTER TABLE events
    ADD CONSTRAINT events_source_description_length_check CHECK (
        source_description IS NULL OR char_length(source_description) <= 10000
    ) NOT VALID;

CREATE TABLE event_copy_enrichments (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    source_hash bytea NOT NULL CHECK (octet_length(source_hash) = 32),
    language char(2) NOT NULL CHECK (language ~ '^[a-z]{2}$'),
    status text NOT NULL DEFAULT 'pending' CHECK (
        status IN ('pending', 'applied', 'stale', 'rejected')
    ),
    model text CHECK (
        model IS NULL OR (btrim(model) <> '' AND char_length(model) <= 160)
    ),
    generated_description text CHECK (
        generated_description IS NULL OR (
            btrim(generated_description) <> ''
            AND char_length(generated_description) <= 4000
        )
    ),
    rejection_reason text CHECK (
        rejection_reason IS NULL OR char_length(rejection_reason) <= 500
    ),
    requested_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, event_id, source_hash, language),
    CONSTRAINT event_copy_enrichments_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CHECK ((status = 'pending') = (completed_at IS NULL)),
    CHECK (
        status <> 'applied'
        OR (generated_description IS NOT NULL AND model IS NOT NULL)
    )
);

CREATE TRIGGER event_copy_enrichments_set_updated_at
BEFORE UPDATE ON event_copy_enrichments
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX event_copy_enrichments_pending_idx
    ON event_copy_enrichments (workspace_id, status, requested_at, id)
    WHERE status = 'pending';
