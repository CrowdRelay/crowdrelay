-- Band listings (§4h-12, build step b).
--
-- A band looking for an agent or a label publishes a profile it composes and
-- can revoke. The point of storing it separately from everything else the
-- workspace holds is the tenant boundary: a label reading a tenant's calendar
-- and fanbase is one tenant reading another's commercial position, and a label
-- reading a listing is the band advertising. Those must not be the same query,
-- so they are not the same table.
--
-- Nothing writes here except the band. No trigger fills a claim from fan rows,
-- no sync infers a city from geography. If a number is in a listing, somebody
-- typed it, which is what makes `redact` in `crowdrelay_domain::listing` a
-- boundary rather than a filter over data the reader could have reached anyway.
--
-- Still workspace-scoped: the listing belongs to the band. Publication is the
-- `visibility` column, not the absence of a `workspace_id`.

CREATE TABLE viryaos_band_listings (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    act_name text NOT NULL CHECK (btrim(act_name) <> '' AND char_length(act_name) <= 160),
    -- Free-form tags, the same shape as a venue's genre and a format's
    -- `genre_fit`: a bias for matching, never a filter that hides a band.
    genre_tags text[] NOT NULL DEFAULT '{}',
    cities text[] NOT NULL DEFAULT '{}',
    published_dates text[] NOT NULL DEFAULT '{}',
    seeking text[] NOT NULL DEFAULT '{}',
    -- Default unlisted: being visible is a decision, never a side effect of
    -- filling the form in.
    visibility text NOT NULL DEFAULT 'unlisted'
        CHECK (visibility IN ('unlisted', 'admitted_readers')),
    -- When the band last published. NULL while unlisted, so "has this ever
    -- been live" is answerable without reading an audit trail.
    published_at timestamptz,
    -- The link is the admission: `visibility='admitted_readers'` means
    -- readable by whoever holds the share URL, not by the open web. The
    -- band admits a reader by handing them the link; unlisting returns the
    -- token's readers to 404, and rotating the token revokes links already
    -- sent. Generated once at row creation; it never rotates on its own.
    share_token uuid NOT NULL DEFAULT gen_random_uuid(),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (array_length(genre_tags, 1) IS NULL OR array_length(genre_tags, 1) <= 12),
    CHECK (array_length(cities, 1) IS NULL OR array_length(cities, 1) <= 40),
    CHECK (array_length(published_dates, 1) IS NULL OR array_length(published_dates, 1) <= 40),
    CHECK (array_length(seeking, 1) IS NULL OR array_length(seeking, 1) <= 8),
    -- A listing cannot be published without a publication time, and an
    -- unlisted one cannot claim to be live. Keeping the two columns honest
    -- here means no reader has to reconcile them.
    CHECK (
        (visibility = 'unlisted' AND published_at IS NULL)
        OR (visibility <> 'unlisted' AND published_at IS NOT NULL)
    )
);

-- One claim on a listing: a number the band chose to show, and what backs it.
--
-- `value` is nullable on purpose. A band may say "we do not know this yet"
-- internally, and the domain's `redact` drops unsupported claims before any
-- outside reader sees them — outside this tenant an absent number would be
-- indistinguishable from a zero, which is the read-model rule applied where
-- the reader cannot ask.
CREATE TABLE viryaos_band_listing_claims (
    workspace_id uuid NOT NULL REFERENCES viryaos_band_listings(workspace_id) ON DELETE CASCADE,
    position integer NOT NULL CHECK (position BETWEEN 0 AND 23),
    label text NOT NULL CHECK (btrim(label) <> '' AND char_length(label) <= 120),
    value bigint,
    -- Matches `MetricValueTier`: reach, intent, banked. A listing whose
    -- supported claims are all `vanity` is the press kit this replaces, and
    -- the domain refuses to publish it.
    tier text NOT NULL CHECK (tier IN ('vanity', 'intermediate', 'downstream')),
    -- Where the number came from, in words the band can be held to. An
    -- unbacked number is refused: the listing's whole value is that its
    -- numbers are true.
    basis text NOT NULL CHECK (btrim(basis) <> '' AND char_length(basis) <= 200),
    PRIMARY KEY (workspace_id, position)
);

-- Readers ask for visible listings, never for one workspace's row.
CREATE INDEX viryaos_band_listings_visible_idx
    ON viryaos_band_listings (visibility, published_at DESC)
    WHERE visibility <> 'unlisted';
