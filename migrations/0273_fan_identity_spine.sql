-- The identity spine (§4e-5): a fan is a person reachable through several
-- verified identifiers — a QR check-in with no email, a ticket buyer email, a
-- Signal install. The only join today is fans.normalized_email, so the same
-- human lands as multiple fan rows and the headline metrics double-count.
--
-- Three tables:
--   fan_identifiers      — verified identifiers attached to a fan. One
--                          (kind, value) belongs to at most one fan, so a
--                          second fan arriving with the same identifier is
--                          detectable instead of silent.
--   fan_merge_candidates — evidence that two fan rows are probably the same
--                          person, parked for a human decision. Never a merge.
--   fan_merges           — the explicit, reversible merge: which rows moved
--                          and which stayed, so an unmerge restores exactly.
--
-- A merged fan keeps its row (and its email, for history) with
-- status='merged' and merged_into_fan_id pointing at the survivor. Reads that
-- filter on the four contact statuses never see it; email resolution goes
-- through fan_identifiers so the old address still reaches the person.

ALTER TABLE fans DROP CONSTRAINT fans_status_check;
ALTER TABLE fans ADD CONSTRAINT fans_status_check
    CHECK (status IN ('pending', 'active', 'unsubscribed', 'suppressed', 'merged'));
ALTER TABLE fans ADD COLUMN merged_into_fan_id uuid;
ALTER TABLE fans ADD COLUMN merged_at timestamptz;
ALTER TABLE fans ADD CONSTRAINT fans_merged_state_check
    CHECK ((status = 'merged') = (merged_into_fan_id IS NOT NULL AND merged_at IS NOT NULL));
ALTER TABLE fans ADD CONSTRAINT fans_merged_into_fk
    FOREIGN KEY (workspace_id, merged_into_fan_id)
    REFERENCES fans (workspace_id, id) ON DELETE RESTRICT;
CREATE INDEX fans_merged_into_idx
    ON fans (workspace_id, merged_into_fan_id)
    WHERE merged_into_fan_id IS NOT NULL;

CREATE TABLE fan_identifiers (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind IN ('email', 'signal_install')),
    value text NOT NULL CHECK (btrim(value) <> ''),
    source text NOT NULL CHECK (btrim(source) <> ''),
    verified_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- The spine's load-bearing constraint: an identifier belongs to at most
    -- one fan. A second fan presenting it is the merge signal, not a write.
    UNIQUE (workspace_id, kind, value),
    CONSTRAINT fan_identifiers_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);
CREATE INDEX fan_identifiers_fan_idx
    ON fan_identifiers (workspace_id, fan_id, kind);

-- Every fan's primary email is also an identifier, so email resolution has
-- one path whether the fan was created yesterday or after a merge moved a
-- second address onto them.
CREATE FUNCTION crowdrelay_fan_primary_email_identifier()
RETURNS trigger AS $$
BEGIN
    INSERT INTO fan_identifiers (
        workspace_id, fan_id, kind, value, source, verified_at
    )
    VALUES (NEW.workspace_id, NEW.id, 'email', NEW.normalized_email, 'fan_created', NEW.created_at)
    ON CONFLICT (workspace_id, kind, value) DO NOTHING;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER fans_primary_email_identifier
AFTER INSERT ON fans
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_fan_primary_email_identifier();

-- Backfill: existing fans' primary emails become identifiers.
INSERT INTO fan_identifiers (workspace_id, fan_id, kind, value, source, verified_at)
SELECT workspace_id, id, 'email', normalized_email, 'backfill_0273', created_at
FROM fans
ON CONFLICT (workspace_id, kind, value) DO NOTHING;

CREATE TABLE fan_merges (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    survivor_fan_id uuid NOT NULL,
    merged_fan_id uuid NOT NULL,
    prior_status text NOT NULL CHECK (btrim(prior_status) <> ''),
    reason text,
    -- moved: {"table_name": ["pk", ...]} — rows re-pointed to the survivor.
    -- retained: same shape — rows left on the merged fan because a unique
    -- constraint would collide or the table is append-only. Unmerge replays
    -- moved and nothing else.
    moved jsonb NOT NULL,
    retained jsonb NOT NULL,
    consents_mirrored integer NOT NULL DEFAULT 0 CHECK (consents_mirrored >= 0),
    merged_by text NOT NULL CHECK (btrim(merged_by) <> ''),
    merged_at timestamptz NOT NULL,
    unmerged_at timestamptz,
    unmerged_by text,
    request_id text,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK (survivor_fan_id <> merged_fan_id),
    CONSTRAINT fan_merges_survivor_fk
        FOREIGN KEY (workspace_id, survivor_fan_id)
        REFERENCES fans (workspace_id, id) ON DELETE RESTRICT,
    CONSTRAINT fan_merges_merged_fk
        FOREIGN KEY (workspace_id, merged_fan_id)
        REFERENCES fans (workspace_id, id) ON DELETE RESTRICT,
    CHECK (unmerged_at IS NULL OR unmerged_at >= merged_at),
    CHECK ((unmerged_at IS NULL) = (unmerged_by IS NULL))
);
CREATE INDEX fan_merges_merged_fan_idx
    ON fan_merges (workspace_id, merged_fan_id, merged_at DESC);
CREATE INDEX fan_merges_open_idx
    ON fan_merges (workspace_id, survivor_fan_id)
    WHERE unmerged_at IS NULL;

CREATE TABLE fan_merge_candidates (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- fan_a/fan_b are stored in canonical (smaller, larger) id order so the
    -- UNIQUE pair can't be recorded twice in either direction.
    fan_id_a uuid NOT NULL,
    fan_id_b uuid NOT NULL,
    evidence jsonb NOT NULL,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'merged', 'dismissed')),
    created_at timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    resolved_merge_id uuid REFERENCES fan_merges (id) ON DELETE SET NULL,
    CHECK (fan_id_a < fan_id_b),
    UNIQUE (workspace_id, fan_id_a, fan_id_b),
    CONSTRAINT fan_merge_candidates_a_fk
        FOREIGN KEY (workspace_id, fan_id_a)
        REFERENCES fans (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT fan_merge_candidates_b_fk
        FOREIGN KEY (workspace_id, fan_id_b)
        REFERENCES fans (workspace_id, id) ON DELETE CASCADE,
    CHECK ((status = 'pending') = (resolved_at IS NULL))
);
CREATE INDEX fan_merge_candidates_pending_idx
    ON fan_merge_candidates (workspace_id, created_at DESC)
    WHERE status = 'pending';
