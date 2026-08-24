-- What happened after a playlist pitch, and whether anybody can prove it.
--
-- A pitcher that counts sends is a spam cannon with a dashboard. The number
-- that matters is placements, and placements are exactly the number somebody
-- has a motive to lie about: the whole playlist-promotion economy runs on
-- screenshots of adds that were removed the following week.
--
-- So a claim and a placement are different rows' worth of certainty in one
-- column. `claimed` is what a curator said. `verified` is what a public read
-- found. `ghosted` is a claim nothing confirmed — not an accusation, and not
-- countable. `withdrawn` is confirmed and then gone inside the window, which is
-- the strongest scam signal in the system.
--
-- The re-check schedule is why this is a table rather than a column: the same
-- placement is read at the claim, again after a week and again after a month,
-- and a read that failed must be distinguishable from a read that found
-- nothing. A dead credential is not evidence that a track is gone.

CREATE TABLE viryaos_playlist_placements (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    opportunity_id uuid NOT NULL,
    state text NOT NULL DEFAULT 'claimed' CHECK (state IN (
        'claimed', 'verified', 'ghosted', 'withdrawn'
    )),
    -- What is supposed to be where. Public identifiers, so any later read can
    -- be repeated by a human with a browser.
    playlist_external_id text NOT NULL
        CHECK (btrim(playlist_external_id) <> '' AND char_length(playlist_external_id) <= 200),
    track_external_id text NOT NULL
        CHECK (btrim(track_external_id) <> '' AND char_length(track_external_id) <= 200),
    claimed_at timestamptz NOT NULL DEFAULT now(),
    -- The last read that actually happened. An unreadable check is not
    -- recorded here, because it is not a read.
    last_observation text CHECK (last_observation IS NULL OR last_observation IN (
        'present', 'absent'
    )),
    last_checked_at timestamptz,
    -- Checkpoints satisfied by a real read: confirmation, week, month. Bounded
    -- here as well as in the domain, because a row claiming nine reads is a row
    -- no code path wrote.
    checks_completed smallint NOT NULL DEFAULT 0 CHECK (checks_completed BETWEEN 0 AND 3),
    settled_at timestamptz,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_playlist_placements_opportunity_fk
        FOREIGN KEY (workspace_id, opportunity_id)
        REFERENCES viryaos_outreach_opportunities (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, id),
    -- One placement per pitch. A curator who claims twice is claiming about the
    -- same thing, and two rows would let one pitch be counted twice.
    UNIQUE (workspace_id, opportunity_id),
    CHECK ((settled_at IS NOT NULL) = (state IN ('ghosted', 'withdrawn'))),
    -- A read time without a read, or the other way round, describes something
    -- that did not happen.
    CHECK ((last_observation IS NULL) = (last_checked_at IS NULL))
);

CREATE TRIGGER viryaos_playlist_placements_set_updated_at
BEFORE UPDATE ON viryaos_playlist_placements
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

-- The cycle asks "which placements are due a read" every few minutes. Partial,
-- because a settled one is never looked at again.
CREATE INDEX viryaos_playlist_placements_live_idx
    ON viryaos_playlist_placements (workspace_id, claimed_at)
    WHERE settled_at IS NULL;

-- Who is behind a target, when it is somebody who runs more than one.
--
-- Suppression has been per-target since 0034, which is right for a bounce and
-- wrong for a withdrawal: one person often runs dozens of playlists, and a
-- withdrawal is a fact about how they operate rather than about the playlist it
-- happened in. Nullable, because most targets are one person with one route and
-- inventing an identity for them would be inventing a fact.
ALTER TABLE viryaos_outreach_targets
    ADD COLUMN curator_identity text
        CHECK (curator_identity IS NULL OR (
            btrim(curator_identity) <> '' AND char_length(curator_identity) <= 200
        ));

-- Suppressing an identity touches every target that shares it.
CREATE INDEX viryaos_outreach_targets_identity_idx
    ON viryaos_outreach_targets (workspace_id, curator_identity)
    WHERE curator_identity IS NOT NULL;
