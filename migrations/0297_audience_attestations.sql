-- Audience attestations (Sprint 4A): a figure a buyer can act on.
--
-- The policy lives in `crowdrelay_domain::attestation`; this is the table
-- behind it. An attestation is a document the platform issued about a tenant's
-- audience — figures measured from this database's own ledger, with the method
-- that produced each one, when it was observed, and a digest over all of it.
--
-- Four things about the shape are deliberate.
--
-- **The figures are stored as issued, not recomputed on read.** An attestation
-- is a snapshot somebody was shown and may have acted on. Recomputing it at
-- read time would silently change a document a label is holding, which is the
-- opposite of the guarantee. Re-measuring produces a NEW attestation with a new
-- digest; the old one stays exactly as issued until it expires or is revoked.
--
-- **The digest is stored and also derivable.** The domain recomputes it from
-- the figures, so a row whose `digest` disagrees with its `figures` is a
-- corrupted row and the verify path says so rather than trusting the column.
-- Storing it anyway makes the common lookup a single indexed read.
--
-- **The signature is what makes it ours.** The digest proves the document is
-- internally consistent; anybody can compute one. The HMAC over the digest
-- proves this platform issued it, and that is the only part a stranger has a
-- reason to believe. It is stored rather than recomputed so a key rotation
-- does not silently invalidate every document already in the wild — a
-- mismatched signature after rotation is a fact the verify path reports, not
-- an error it hides.
--
-- **Revocation is a timestamp, never a delete.** A band that revokes an
-- attestation wants the link to stop working; it does not want the record of
-- having issued it to disappear. A deleted row would also make a held document
-- unverifiable rather than revoked, and "we cannot find this" reads to a
-- sceptical reader exactly like "this was forged".

CREATE TABLE viryaos_attestations (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- Denormalised from the workspace at issue time on purpose: the document
    -- states the name it was issued under, and a later rename must not rewrite
    -- a document somebody is holding.
    act_name text NOT NULL CHECK (
        btrim(act_name) <> '' AND char_length(act_name) <= 200
    ),

    -- The issued figures, exactly as the domain serialised them. Read back
    -- through `serde` into `Vec<AttestedFigure>`; never edited in place.
    figures jsonb NOT NULL CHECK (jsonb_typeof(figures) = 'array'),

    issued_at timestamptz NOT NULL,
    valid_until timestamptz NOT NULL,

    -- Hex SHA-256 over the canonical form. 64 lowercase hex characters.
    digest text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    -- Hex HMAC-SHA256 over the digest, under the server's attestation key.
    signature text NOT NULL CHECK (signature ~ '^[0-9a-f]{64}$'),

    -- What a link-holder presents. Rotatable per attestation, which is how a
    -- band un-sends a link it sent to the wrong person.
    share_token uuid NOT NULL DEFAULT gen_random_uuid(),

    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),

    CHECK (valid_until > issued_at),
    -- A revocation cannot predate the issue it revokes.
    CHECK (revoked_at IS NULL OR revoked_at >= issued_at),
    UNIQUE (share_token)
);

-- The verify path's lookup: a stranger presents a digest from a document they
-- were handed and asks whether we issued it. Unique because a digest covers the
-- act name, the figures and both timestamps — two issues that collide here are
-- byte-identical documents, and returning either is the same answer.
CREATE UNIQUE INDEX viryaos_attestations_digest_uq
    ON viryaos_attestations (digest);

-- The tenant's own list, newest first.
CREATE INDEX viryaos_attestations_workspace_idx
    ON viryaos_attestations (workspace_id, issued_at DESC, id DESC);

-- Figures are append-once. An attestation is a snapshot somebody may be
-- holding, so the only legal mutations are revoking it and rotating its share
-- token; editing what it says is the one thing this table exists to prevent,
-- and a trigger says so rather than a code comment nobody reads at 2am.
CREATE FUNCTION viryaos_attestations_are_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
       OR NEW.act_name IS DISTINCT FROM OLD.act_name
       OR NEW.figures IS DISTINCT FROM OLD.figures
       OR NEW.issued_at IS DISTINCT FROM OLD.issued_at
       OR NEW.valid_until IS DISTINCT FROM OLD.valid_until
       OR NEW.digest IS DISTINCT FROM OLD.digest
       OR NEW.signature IS DISTINCT FROM OLD.signature
    THEN
        RAISE EXCEPTION
            'an issued attestation cannot be edited; issue a new one instead'
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER viryaos_attestations_are_immutable
BEFORE UPDATE ON viryaos_attestations
FOR EACH ROW EXECUTE FUNCTION viryaos_attestations_are_immutable();
